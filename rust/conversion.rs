use crate::{
    archive::safe_path,
    error::{invalid, Error, Result},
};
use arrow_array::{
    builder::{BinaryBuilder, StringBuilder},
    Array, BinaryArray, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::{
    arrow::{arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter},
    basic::{Compression, ZstdLevel},
    file::properties::{EnabledStatistics, WriterProperties},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

const MAX_TXT_BYTES: u64 = 64 * 1024 * 1024;
const BLOCK_BYTES: u64 = 16 * 1024 * 1024;
const BLOCK_FILES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    #[value(name = "7z")]
    SevenZip,
    #[value(name = "tar.zst")]
    TarZst,
    Parquet,
}

impl Format {
    pub const ALL: [Self; 3] = [Self::SevenZip, Self::TarZst, Self::Parquet];

    pub fn extension(self) -> &'static str {
        match self {
            Self::SevenZip => ".7z",
            Self::TarZst => ".tar.zst",
            Self::Parquet => ".parquet",
        }
    }

    pub fn detect(path: &Path) -> Result<Self> {
        let name = file_name(path)?.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|format| name.ends_with(format.extension()))
            .ok_or_else(|| invalid("Choose a .7z, .tar.zst or .parquet file"))
    }
}

#[derive(Clone, Copy)]
pub struct Range {
    first: usize,
    last: usize,
}

impl std::str::FromStr for Range {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let (first, last) = value.split_once('-').unwrap_or((value, value));
        let parse = |s: &str| s.trim().parse::<usize>().ok().filter(|i| *i > 0);
        match (parse(first), parse(last)) {
            (Some(first), Some(last)) if first <= last => Ok(Self { first, last }),
            _ => Err(invalid(
                "Enter a file number or an inclusive range such as 2-10",
            )),
        }
    }
}

#[derive(Clone)]
struct SourceFile {
    name: String,
    size: u64,
    modified: SystemTime,
}

impl SourceFile {
    fn read(&self, root: &Path, stop: &dyn Fn() -> bool) -> Result<Vec<u8>> {
        let path = safe_path(root, &self.name)?;
        let metadata = fs::symlink_metadata(&path)?;
        if is_link(&metadata) || !path.canonicalize()?.starts_with(root) {
            return Err(invalid("A selected TXT path changed after preview"));
        }
        let file = File::open(path)?;
        self.check(&file)?;
        let mut contents = Vec::with_capacity(self.size as usize);
        CancelReader(&file, stop)
            .take(self.size + 1)
            .read_to_end(&mut contents)?;
        self.check(&file)?;
        if contents.len() as u64 != self.size {
            return Err(invalid("A selected TXT changed after preview"));
        }
        Ok(contents)
    }

    fn check(&self, file: &File) -> Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.len() != self.size
            || metadata.modified()? != self.modified
        {
            return Err(invalid(&format!(
                "Selected TXT changed after preview: {}",
                self.name
            )));
        }
        Ok(())
    }
}

pub struct Library {
    root: PathBuf,
    files: Vec<SourceFile>,
}

impl Library {
    pub fn read(root: &Path, stop: &dyn Fn() -> bool) -> Result<Self> {
        let root = match root.canonicalize() {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    root: root.to_owned(),
                    files: Vec::new(),
                })
            }
            Err(error) => return Err(error.into()),
        };
        let mut files = Vec::new();
        for folder in ["novel", "works/novel"] {
            let path = safe_path(&root, folder)?;
            if let Ok(metadata) = fs::symlink_metadata(&path) {
                if is_link(&metadata) || !path.canonicalize()?.starts_with(&root) {
                    return Err(invalid("A saved TXT folder is outside the archive"));
                }
                if !metadata.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(path)? {
                    cancelled(stop)?;
                    let path = entry?.path();
                    if !is_txt(&path) {
                        continue;
                    }
                    let metadata = fs::symlink_metadata(&path)?;
                    if metadata.is_file() && !is_link(&metadata) {
                        add_source(&root, &path, &metadata, &mut files)?;
                    }
                }
            }
        }
        files.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { root, files })
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|file| file.name.as_str())
    }

    pub fn exporting(
        &self,
        selected: &[usize],
        destination: Option<&Path>,
        format: Format,
    ) -> Result<Plan> {
        let files = selected
            .iter()
            .map(|&index| {
                self.files
                    .get(index)
                    .cloned()
                    .ok_or_else(|| invalid("A selection is outside the saved TXT list"))
            })
            .collect::<Result<Vec<_>>>()?;
        if files.is_empty() {
            return Err(invalid("Select at least one saved TXT"));
        }
        let destination = checked_destination(destination, &self.root, format, false)?;
        Plan::for_files(
            self.root.clone(),
            destination,
            format,
            self.root.clone(),
            files,
            None,
        )
    }
}

enum Input {
    Text {
        root: PathBuf,
        files: Vec<SourceFile>,
    },
    Archive {
        size: u64,
        modified: SystemTime,
    },
}

pub struct Plan {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub format: Format,
    pub input_bytes: u64,
    input: Input,
    pub range: Option<(usize, usize, usize)>,
}

#[derive(Serialize)]
pub struct Report {
    pub files: usize,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub destination: PathBuf,
}

impl Plan {
    pub fn exporting(
        source: &Path,
        destination: Option<&Path>,
        format: Format,
        range: Option<Range>,
        stop: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let metadata = fs::symlink_metadata(source)?;
        if is_link(&metadata) {
            return Err(invalid("Choose a real TXT file or folder"));
        }
        let source = source.canonicalize()?;
        let destination = checked_destination(destination, &source, format, false)?;
        let mut files = Vec::new();
        let root = if metadata.is_dir() {
            let mut pending = vec![source.clone()];
            while let Some(folder) = pending.pop() {
                cancelled(stop)?;
                for entry in fs::read_dir(folder)? {
                    cancelled(stop)?;
                    let path = entry?.path();
                    let metadata = fs::symlink_metadata(&path)?;
                    if is_link(&metadata) {
                        continue;
                    }
                    if metadata.is_dir() {
                        pending.push(path);
                    } else if metadata.is_file() && is_txt(&path) {
                        add_source(&source, &path, &metadata, &mut files)?;
                    }
                }
            }
            source.clone()
        } else if metadata.is_file() && is_txt(&source) {
            let root = parent(&source)?.to_owned();
            add_source(&root, &source, &metadata, &mut files)?;
            root
        } else {
            return Err(invalid(
                "Choose a TXT file or a folder containing TXT files",
            ));
        };
        files.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let all = files.len();
        if all == 0 {
            return Err(invalid("No TXT files found in the selected folder"));
        }
        let Range { first, last } = range.unwrap_or(Range {
            first: 1,
            last: all,
        });
        if last > all {
            return Err(invalid(&format!(
                "Range ends at {last}, but only {all} TXT files were found"
            )));
        }
        files.truncate(last);
        files.drain(..first - 1);
        Self::for_files(
            source,
            destination,
            format,
            root,
            files,
            Some((first, last, all)),
        )
    }

    fn for_files(
        source: PathBuf,
        destination: PathBuf,
        format: Format,
        root: PathBuf,
        files: Vec<SourceFile>,
        range: Option<(usize, usize, usize)>,
    ) -> Result<Self> {
        for file in &files {
            check_size(file.size)?;
        }
        let input_bytes = files.iter().map(|file| file.size).sum();
        Ok(Self {
            source,
            destination,
            format,
            input_bytes,
            input: Input::Text { root, files },
            range,
        })
    }

    pub fn restoring(source: &Path, destination: Option<&Path>) -> Result<Self> {
        let format = Format::detect(source)?;
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.is_file() || is_link(&metadata) {
            return Err(invalid("Choose a regular archive file"));
        }
        let source = source.canonicalize()?;
        let destination = checked_destination(destination, &source, format, true)?;
        let size = metadata.len();
        Ok(Self {
            source,
            destination,
            format,
            input_bytes: size,
            range: None,
            input: Input::Archive {
                size,
                modified: metadata.modified()?,
            },
        })
    }

    pub fn edges(&self) -> Option<(&str, &str)> {
        let Input::Text { files, .. } = &self.input else {
            return None;
        };
        Some((&files.first()?.name, &files.last()?.name))
    }

    pub fn file_count(&self) -> Option<usize> {
        match &self.input {
            Input::Text { files, .. } => Some(files.len()),
            Input::Archive { .. } => None,
        }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        let files = match &self.input {
            Input::Text { files, .. } => files.as_slice(),
            Input::Archive { .. } => &[],
        };
        files.iter().map(|file| file.name.as_str())
    }

    pub fn execute(
        &self,
        stop: &dyn Fn() -> bool,
        mut on_file: impl FnMut(usize),
    ) -> Result<Report> {
        cancelled(stop)?;
        let (files, output_bytes) = match &self.input {
            Input::Text { root, files } => {
                let mut output = tempfile::NamedTempFile::new_in(parent(&self.destination)?)?;
                match self.format {
                    Format::SevenZip => {
                        write_7z(output.as_file_mut(), root, files, stop, &mut on_file)?
                    }
                    Format::TarZst => {
                        write_tar(output.as_file_mut(), root, files, stop, &mut on_file)?
                    }
                    Format::Parquet => {
                        write_parquet(output.as_file_mut(), root, files, stop, &mut on_file)?
                    }
                }
                output.as_file().sync_all()?;
                let bytes = output.as_file().metadata()?.len();
                cancelled(stop)?;
                output
                    .persist_noclobber(&self.destination)
                    .map_err(|error| error.error)?;
                (files.len(), bytes)
            }
            Input::Archive { size, modified } => {
                let input = File::open(&self.source)?;
                let check = || -> Result<()> {
                    let metadata = input.metadata()?;
                    if !metadata.is_file()
                        || metadata.len() != *size
                        || metadata.modified()? != *modified
                    {
                        return Err(invalid("The archive changed after preview"));
                    }
                    Ok(())
                };
                check()?;
                let stage = tempfile::tempdir_in(parent(&self.destination)?)?;
                let mut output = Restore {
                    root: stage.path(),
                    count: 0,
                    bytes: 0,
                    stop,
                    on_file,
                };
                match self.format {
                    Format::SevenZip => read_7z(input.try_clone()?, &mut output)?,
                    Format::TarZst => read_tar(&input, &mut output)?,
                    Format::Parquet => read_parquet(input.try_clone()?, &mut output)?,
                }
                if output.count == 0 {
                    return Err(invalid("The archive contains no TXT files"));
                }
                check()?;
                cancelled(stop)?;
                publish_directory(stage.path(), &self.destination)?;
                let result = (output.count, output.bytes);
                let _ = stage.keep();
                result
            }
        };
        Ok(Report {
            files,
            input_bytes: self.input_bytes,
            output_bytes,
            destination: self.destination.clone(),
        })
    }
}

pub fn default_destination(source: &Path, format: Format, restore: bool) -> Result<PathBuf> {
    let name = file_name(source)?;
    let leaf = if restore {
        let stem = name
            .get(..name.len().saturating_sub(format.extension().len()))
            .filter(|s| !s.is_empty());
        format!("{}-txt", stem.unwrap_or("restored"))
    } else {
        let stem = if is_txt(source) && !source.is_dir() {
            name.get(..name.len() - 4).unwrap_or(name)
        } else {
            name
        };
        format!("{}{ext}", stem, ext = format.extension())
    };
    Ok(source.with_file_name(leaf))
}

fn checked_destination(
    destination: Option<&Path>,
    source: &Path,
    format: Format,
    restore: bool,
) -> Result<PathBuf> {
    let destination = match destination {
        Some(path) => path.to_owned(),
        None => default_destination(source, format, restore)?,
    };
    let name = file_name(&destination)?;
    if !restore && !name.to_ascii_lowercase().ends_with(format.extension()) {
        return Err(invalid(&format!(
            "The output name must end in {}",
            format.extension()
        )));
    }
    validate_component(name)?;
    let destination = parent(&destination)?.canonicalize()?.join(name);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(invalid(
                "The output already exists. Choose a new file or folder name",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(destination)
}

fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        })
        .ok_or_else(|| invalid("Choose a path with a parent folder"))
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("A UTF-8 file or folder name is required"))
}

fn is_txt(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

fn validate_component(name: &str) -> Result<()> {
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let dos_port = ["COM", "LPT"].iter().any(|prefix| {
        base.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*'])
        || name.chars().any(char::is_control)
        || dos_port
        || matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
    {
        return Err(invalid("A file or folder name is unsafe or not portable"));
    }
    Ok(())
}

fn add_source(
    root: &Path,
    path: &Path,
    metadata: &Metadata,
    files: &mut Vec<SourceFile>,
) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| invalid("TXT path is outside the source folder"))?;
    let name = relative
        .to_str()
        .ok_or_else(|| invalid("A TXT path is not UTF-8"))?;
    #[cfg(windows)]
    let name = name.replace('\\', "/");
    #[cfg(not(windows))]
    let name = name.to_owned();
    for component in name.split('/') {
        validate_component(component)?;
    }
    files.push(SourceFile {
        name,
        size: metadata.len(),
        modified: metadata.modified()?,
    });
    Ok(())
}

fn check_size(size: u64) -> Result<()> {
    if size > MAX_TXT_BYTES {
        return Err(invalid("A TXT file exceeds 64 MiB"));
    }
    Ok(())
}

fn cancelled(stop: &dyn Fn() -> bool) -> Result<()> {
    if stop() {
        Err(Error::Stop("TXT conversion cancelled".into()))
    } else {
        Ok(())
    }
}

struct CancelReader<'a, R>(R, &'a dyn Fn() -> bool);

impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        cancelled(self.1).map_err(io::Error::other)?;
        self.0.read(buf)
    }
}

fn codec_error(error: impl std::fmt::Display) -> Error {
    invalid(&format!("TXT archive error: {error}"))
}

fn write_tar(
    output: &mut File,
    root: &Path,
    files: &[SourceFile],
    stop: &dyn Fn() -> bool,
    on_file: &mut dyn FnMut(usize),
) -> Result<()> {
    let mut encoder = zstd::stream::write::Encoder::new(output, 9)?;
    encoder.include_checksum(true)?;
    let mut archive = tar::Builder::new(encoder);
    for (i, file) in files.iter().enumerate() {
        let body = file.read(root, stop)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(file.size);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        archive.append_data(&mut header, &file.name, CancelReader(&body[..], stop))?;
        on_file(i + 1);
    }
    cancelled(stop)?;
    archive.into_inner()?.finish()?;
    Ok(())
}

fn blocks(files: &[SourceFile]) -> impl Iterator<Item = &[SourceFile]> {
    let mut rest = files;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut bytes = rest[0].size;
        let count = 1 + rest[1..]
            .iter()
            .take(BLOCK_FILES - 1)
            .take_while(|file| {
                bytes += file.size;
                bytes <= BLOCK_BYTES
            })
            .count();
        let (block, remaining) = rest.split_at(count);
        rest = remaining;
        Some(block)
    })
}

fn write_7z(
    output: &mut File,
    root: &Path,
    files: &[SourceFile],
    stop: &dyn Fn() -> bool,
    on_file: &mut dyn FnMut(usize),
) -> Result<()> {
    use sevenz_rust2::{
        encoder_options::PpmdOptions, ArchiveEntry, ArchiveWriter, EncoderConfiguration,
        SourceReader,
    };
    let mut archive = ArchiveWriter::new(output).map_err(codec_error)?;
    archive.set_content_methods(vec![EncoderConfiguration::from(PpmdOptions::from_level(7))]);
    let mut count = 0;
    for block in blocks(files) {
        let mut entries = Vec::new();
        let mut bodies = Vec::new();
        for file in block {
            let body = file.read(root, stop)?;
            // Store zero-byte TXT as zero-length streams too. This keeps them as regular files
            // in readers that interpret the empty-entry flags written by sevenz-rust2 0.20.
            entries.push(ArchiveEntry::new_file(&file.name));
            bodies.push(SourceReader::new(CancelReader(Cursor::new(body), stop)));
        }
        archive
            .push_archive_entries(entries, bodies)
            .map_err(codec_error)?;
        count += block.len();
        on_file(count);
    }
    archive.finish()?;
    Ok(())
}

fn write_parquet(
    output: &mut File,
    root: &Path,
    files: &[SourceFile],
    stop: &dyn Fn() -> bool,
    on_file: &mut dyn FnMut(usize),
) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("relative_path", DataType::Utf8, false),
        Field::new("content", DataType::Binary, false),
        Field::new("sha256", DataType::Utf8, false),
    ]));
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(19).map_err(codec_error)?,
        ))
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_max_row_group_bytes(Some(BLOCK_BYTES as usize))
        .build();
    let mut writer =
        ArrowWriter::try_new(output, schema.clone(), Some(properties)).map_err(codec_error)?;
    let mut count = 0;
    for block in blocks(files) {
        let mut names = StringBuilder::new();
        let mut bodies = BinaryBuilder::new();
        let mut hashes = StringBuilder::new();
        for file in block {
            let body = file.read(root, stop)?;
            names.append_value(&file.name);
            hashes.append_value(format!("{:x}", Sha256::digest(&body)));
            bodies.append_value(body);
        }
        cancelled(stop)?;
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(names.finish()),
                Arc::new(bodies.finish()),
                Arc::new(hashes.finish()),
            ],
        )
        .map_err(codec_error)?;
        writer.write(&batch).map_err(codec_error)?;
        count += block.len();
        on_file(count);
    }
    writer.close().map_err(codec_error)?;
    Ok(())
}

struct Restore<'a, F> {
    root: &'a Path,
    count: usize,
    bytes: u64,
    stop: &'a dyn Fn() -> bool,
    on_file: F,
}

impl<F: FnMut(usize)> Restore<'_, F> {
    fn write(&mut self, name: &str, size: u64, data: impl Read) -> Result<()> {
        cancelled(self.stop)?;
        check_size(size)?;
        for component in name.split('/') {
            validate_component(component)?;
        }
        let path = safe_path(self.root, name)?;
        if !is_txt(&path) {
            return Err(invalid("Only TXT entries can be restored"));
        }
        fs::create_dir_all(parent(&path)?)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let written = io::copy(&mut CancelReader(data, self.stop).take(size + 1), &mut file)?;
        if written != size {
            return Err(invalid("A TXT archive entry has an incorrect size"));
        }
        file.flush()?;
        self.count += 1;
        self.bytes += written;
        (self.on_file)(self.count);
        Ok(())
    }
}

fn read_tar(input: &File, output: &mut Restore<'_, impl FnMut(usize)>) -> Result<()> {
    let mut archive = tar::Archive::new(zstd::stream::read::Decoder::new(input)?);
    for entry in archive.entries()? {
        cancelled(output.stop)?;
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            return Err(invalid("The TAR contains a link or other special entry"));
        }
        let name = std::str::from_utf8(&entry.path_bytes())
            .map_err(codec_error)?
            .to_owned();
        output.write(&name, entry.size(), entry)?;
    }
    io::copy(
        &mut CancelReader(archive.into_inner(), output.stop),
        &mut io::sink(),
    )?;
    Ok(())
}

fn read_7z(input: File, output: &mut Restore<'_, impl FnMut(usize)>) -> Result<()> {
    use sevenz_rust2::{ArchiveReader, Password};
    let mut archive = ArchiveReader::new(input, Password::empty()).map_err(codec_error)?;
    archive
        .for_each_entries(|entry, data| {
            cancelled(output.stop).map_err(io::Error::other)?;
            let attributes = entry.windows_attributes();
            if entry.is_anti_item
                || attributes & 0x400 != 0
                || (attributes >> 16) & 0xf000 == 0xa000
            {
                return Err(io::Error::other("The 7z archive contains a link or anti-item").into());
            }
            if !entry.is_directory() {
                output
                    .write(entry.name(), entry.size(), data)
                    .map_err(io::Error::other)?;
            }
            Ok(true)
        })
        .map_err(codec_error)
}

fn read_parquet(input: File, output: &mut Restore<'_, impl FnMut(usize)>) -> Result<()> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(input).map_err(codec_error)?;
    for (name, expected) in [
        ("relative_path", DataType::Utf8),
        ("content", DataType::Binary),
        ("sha256", DataType::Utf8),
    ] {
        if reader
            .schema()
            .field_with_name(name)
            .ok()
            .map(Field::data_type)
            != Some(&expected)
        {
            return Err(invalid("Parquet must contain relative_path, content and sha256 columns in the OpenPixiv TXT schema"));
        }
    }
    for batch in reader.with_batch_size(32).build().map_err(codec_error)? {
        cancelled(output.stop)?;
        let batch = batch.map_err(codec_error)?;
        let names = column::<StringArray>(&batch, "relative_path")?;
        let bodies = column::<BinaryArray>(&batch, "content")?;
        let hashes = column::<StringArray>(&batch, "sha256")?;
        for i in 0..batch.num_rows() {
            let body = bodies.value(i);
            check_size(body.len() as u64)?;
            if format!("{:x}", Sha256::digest(body)) != hashes.value(i) {
                return Err(invalid("Parquet TXT checksum mismatch"));
            }
            output.write(names.value(i), body.len() as u64, body)?;
        }
    }
    Ok(())
}

fn column<'a, T: Array + 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .filter(|array| array.null_count() == 0)
        .and_then(|array| array.as_any().downcast_ref::<T>())
        .ok_or_else(|| invalid("Missing, null or invalid Parquet TXT column"))
}

fn publish_directory(stage: &Path, destination: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        stage,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)?;
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        if fs::symlink_metadata(destination).is_ok() {
            return Err(invalid("The output already exists"));
        }
        fs::rename(stage, destination)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn three_formats_round_trip_selected_txt_bytes_and_paths() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("03")).unwrap();
        let mut body = b"\xef\xbb\xbf".to_vec();
        body.extend_from_slice("日本語\r\nline \r\n".as_bytes());
        body.extend_from_slice(b"\xff\0");
        let expected: [(&str, &[u8]); 2] = [("02.txt", b""), ("03/本文.TXT", &body)];
        fs::write(source.join("01.txt"), b"not selected").unwrap();
        fs::write(source.join("ignored.json"), b"not TXT").unwrap();
        for (path, bytes) in expected {
            fs::write(source.join(path), bytes).unwrap();
        }
        let range = Some("2-3".parse().unwrap());
        for format in Format::ALL {
            let archive = temp.path().join(format!("selected{}", format.extension()));
            let dest = temp.path().join(format!("restored{}", format.extension()));
            let plan = Plan::exporting(&source, Some(&archive), format, range, &|| false).unwrap();
            assert_eq!(plan.range, Some((2, 3, 3)));
            assert_eq!(plan.edges(), Some(("02.txt", "03/本文.TXT")));
            assert!(!archive.exists(), "preview must not write an output");
            let result = plan.execute(&|| false, |_| {}).unwrap();
            assert_eq!(result.files, 2);
            let restored = Plan::restoring(&archive, Some(&dest))
                .unwrap()
                .execute(&|| false, |_| {})
                .unwrap();
            assert_eq!(restored.files, 2);
            for (path, bytes) in expected {
                assert_eq!(
                    fs::read(dest.join(path)).unwrap(),
                    bytes,
                    "{format:?}: {path}"
                );
                assert_eq!(fs::read(source.join(path)).unwrap(), bytes);
            }
            assert!(!dest.join("01.txt").exists());
            assert!(!dest.join("ignored.json").exists());
        }
    }

    #[test]
    fn cancelled_or_conflicting_outputs_never_replace_existing_data() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("single.txt");
        let archive = temp.path().join("single.tar.zst");
        fs::write(&source, b"body").unwrap();
        let plan =
            Plan::exporting(&source, Some(&archive), Format::TarZst, None, &|| false).unwrap();
        let stop = Cell::new(false);
        assert!(plan.execute(&|| stop.get(), |_| stop.set(true)).is_err());
        assert!(!archive.exists());
        plan.execute(&|| false, |_| {}).unwrap();
        let dest = temp.path().join("existing");
        let restore = Plan::restoring(&archive, Some(&dest)).unwrap();
        fs::create_dir(&dest).unwrap();
        assert!(restore.execute(&|| false, |_| {}).is_err());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
        assert_eq!(fs::read(source).unwrap(), b"body");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 3);
    }
}
