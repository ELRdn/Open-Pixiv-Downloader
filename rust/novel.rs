//! Conservative port of the MIT Python renderer. Source text is always retained.
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
struct Token<'a> {
    kind: &'a str,
    raw: &'a str,
    value: &'a str,
    extra: &'a str,
}
fn integer(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit())
}
fn artwork(s: &str) -> bool {
    let parts: Vec<_> = s.split('-').collect();
    (parts.len() == 1 || parts.len() == 2) && parts.iter().all(|s| integer(s))
}
fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut closings = HashMap::new();
    let mut stack = Vec::new();
    for (i, c) in text.char_indices() {
        if c == '[' {
            stack.push(i);
        } else if c == ']' {
            if let Some(start) = stack.pop() {
                closings.insert(start, i);
            }
        }
    }
    let mut out = Vec::new();
    let mut pos = 0;
    let mut pending = 0;
    while pos < text.len() {
        let mut found = None;
        let mut end = closings.get(&pos).copied();
        if text[pos..].starts_with("[newpage]") {
            end = Some(pos + 8);
            found = Some(Token {
                kind: "newpage",
                raw: &text[pos..pos + 9],
                value: "",
                extra: "",
            });
        } else if let Some(e) = end {
            let raw = &text[pos..=e];
            for kind in ["rb", "jumpuri", "emphasismark"] {
                if let Some(body) = raw
                    .strip_prefix(&format!("[[{kind}:"))
                    .and_then(|s| s.strip_suffix("]]"))
                {
                    if let Some((value, extra)) = body.split_once('>') {
                        if !value.trim().is_empty() && !extra.trim().is_empty() {
                            found = Some(Token {
                                kind,
                                raw,
                                value: value.trim(),
                                extra: extra.trim(),
                            });
                        }
                    }
                    break;
                }
            }
            if found.is_none() {
                for kind in ["chapter", "jump", "pixivimage", "uploadedimage", "b", "i"] {
                    if let Some(value) = raw
                        .strip_prefix(&format!("[{kind}:"))
                        .and_then(|s| s.strip_suffix(']'))
                    {
                        let value = value.trim();
                        let valid = match kind {
                            "jump" | "uploadedimage" => integer(value),
                            "pixivimage" => artwork(value),
                            _ => true,
                        };
                        if valid {
                            found = Some(Token {
                                kind,
                                raw,
                                value,
                                extra: "",
                            });
                        }
                        break;
                    }
                }
            }
        }
        if let Some(token) = found {
            if pending < pos {
                out.push(Token {
                    kind: "text",
                    raw: &text[pending..pos],
                    value: "",
                    extra: "",
                });
            }
            out.push(token);
            pos = end.unwrap() + 1;
            pending = pos;
        } else {
            pos = end.map_or_else(
                || pos + text[pos..].chars().next().unwrap().len_utf8(),
                |e| e + 1,
            );
        }
    }
    if pending < text.len() {
        out.push(Token {
            kind: "text",
            raw: &text[pending..],
            value: "",
            extra: "",
        });
    }
    out
}
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
fn http_link(s: &str) -> bool {
    !s.chars().any(|c| c.is_whitespace() || c.is_control())
        && !s.contains('\\')
        && url::Url::parse(s).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host_str().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
}
fn inline(text: &str, depth: usize) -> String {
    if depth >= 16 {
        return escape(text);
    }
    tokens(text)
        .iter()
        .map(|t| inline_token(t, depth))
        .collect()
}
fn inline_token(t: &Token<'_>, depth: usize) -> String {
    if depth >= 16 {
        return escape(t.raw);
    }
    match t.kind {
        "rb" => format!(
            "<ruby>{}<rp>（</rp><rt>{}</rt><rp>）</rp></ruby>",
            inline(t.value, depth + 1),
            escape(t.extra)
        ),
        "b" | "i" => {
            let tag = if t.kind == "b" { "strong" } else { "em" };
            format!("<{tag}>{}</{tag}>", inline(t.value, depth + 1))
        }
        "jumpuri" if http_link(t.extra) => format!(
            "<a href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
            escape(t.extra),
            escape(t.value)
        ),
        "emphasismark"
            if t.extra.chars().count() == 1 && !t.extra.chars().next().unwrap().is_control() =>
        {
            let mark = format!("\\{:06x}", t.extra.chars().next().unwrap() as u32);
            format!(
                "<span style=\"{}\">{}</span>",
                escape(&format!(
                    "text-emphasis:\"{mark}\";-webkit-text-emphasis:\"{mark}\""
                )),
                inline(t.value, depth + 1)
            )
        }
        _ => escape(t.raw),
    }
}
fn plain(text: &str, depth: usize, body_only: bool) -> String {
    if depth >= 16 {
        return text.into();
    }
    tokens(text)
        .iter()
        .map(|t| match t.kind {
            "rb" | "b" | "i" | "emphasismark" => plain(t.value, depth + 1, body_only),
            "chapter" => format!("\n\n{}\n\n", plain(t.value, depth + 1, body_only)),
            "pixivimage" | "uploadedimage" if body_only => String::new(),
            "newpage" => "\n\n".into(),
            "jumpuri" => format!("{} ({})", t.value, t.extra),
            "jump" => format!("[{}ページへ]", t.value),
            _ => t.raw.into(),
        })
        .collect()
}
pub fn plain_text(text: &str) -> String {
    plain(text, 0, false)
}
pub fn body_text(text: &str) -> String {
    plain(text, 0, true)
}
pub fn image_refs(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokens(text)
        .iter()
        .filter(|t| matches!(t.kind, "pixivimage" | "uploadedimage"))
        .map(|t| format!("{}:{}", t.kind, t.value))
        .filter(|key| seen.insert(key.clone()))
        .collect()
}
fn local_image(s: &str) -> bool {
    !s.is_empty()
        && !s.contains(['\\', ':', '?', '#', '%'])
        && !s.chars().any(char::is_control)
        && !s.split('/').any(|p| matches!(p, "" | "." | ".."))
        && crate::archive::extension(s).is_ok()
}
pub fn render(text: &str, title: &str, author: &str, images: &HashMap<String, String>) -> String {
    let tokens = tokens(text);
    let total = 1 + tokens.iter().filter(|t| t.kind == "newpage").count();
    let mut page = 1;
    let mut chapter = 0;
    let mut body = "<section class=\"page\" id=\"page-1\"><div class=\"text\">".to_string();
    for t in tokens {
        body.push_str(&match t.kind {
            "newpage" => { page += 1; format!("</div></section><section class=\"page\" id=\"page-{page}\"><div class=\"text\">") }
            "chapter" => { chapter += 1; format!("<h2 id=\"chapter-{chapter}\">{}</h2>", inline(t.value, 0)) }
            "jump" if t.value.parse::<usize>().is_ok_and(|n| n <= total) => format!("<a href=\"#page-{}\">{}ページへ</a>", t.value, t.value),
            "pixivimage" | "uploadedimage" => {
                let key = format!("{}:{}", t.kind, t.value);
                if let Some(path) = images.get(&key).filter(|p| local_image(p)) {
                    let encoded: String = path.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
                    format!("<figure><img src=\"{encoded}\" alt=\"{}\" loading=\"lazy\"><figcaption>{}</figcaption></figure>", escape(t.raw), escape(t.raw))
                } else if t.kind == "pixivimage" {
                    format!("<p class=\"unresolved\"><a href=\"https://www.pixiv.net/artworks/{}\" rel=\"noreferrer noopener\">{}（挿絵未保存・作品ページ）</a></p>", t.value.split('-').next().unwrap(), escape(t.raw))
                } else { format!("<p class=\"unresolved\">{}（挿絵未保存）</p>", escape(t.raw)) }
            }
            _ => inline_token(&t, 0),
        });
    }
    format!("<!doctype html>\n<html lang=\"ja\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta name=\"referrer\" content=\"no-referrer\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><title>{}</title><style>{}</style></head><body><header><h1>{}</h1><p>{}</p></header><main>{body}</div></section></main></body></html>\n",
        escape(if title.is_empty() { "無題" } else { title }), STYLE, escape(title), escape(author))
}
const STYLE: &str = ":root{color-scheme:light dark;font-family:serif;line-height:1.95}body{max-width:48rem;margin:auto;padding:2rem 1.2rem 5rem}.text{white-space:pre-wrap;overflow-wrap:anywhere}h2{margin:2rem 0 1rem}figure{white-space:normal;margin:2rem 0}img{max-width:100%;height:auto}.unresolved{color:#888}.page+.page{border-top:1px solid #888;padding-top:2rem;margin-top:3rem}rt{font-size:.55em}@media print{.page+.page{break-before:page;border:0}figure{break-inside:avoid}}";

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structure_and_nested_ruby() {
        let out = render(
            "[chapter:第一章 [[rb:海>うみ]]][b:出発][i:帰還][jump:2][newpage]二頁",
            "試作",
            "作者",
            &HashMap::new(),
        );
        for s in [
            "<ruby>海",
            "<rt>うみ</rt>",
            "<strong>出発</strong>",
            "<em>帰還</em>",
            "href=\"#page-2\"",
            "id=\"page-2\"",
        ] {
            assert!(out.contains(s), "{s}");
        }
    }
    #[test]
    fn preserve_unknown_and_plain_derivative() {
        let s = "[chapter:章][[rb:漢字>かんじ]][b:太字][newpage][unknown:x][uploadedimage:12]";
        assert_eq!(
            plain_text(s),
            "\n\n章\n\n漢字太字\n\n[unknown:x][uploadedimage:12]"
        );
        assert_eq!(plain_text("[unknown:[b:literal]]"), "[unknown:[b:literal]]");
    }
    #[test]
    fn offline_html_and_unsafe_links() {
        let out = render(
            "<script>x</script>[[jumpuri:x>javascript:alert(1)]][uploadedimage:1]",
            "</title><script>",
            "",
            &HashMap::from([("uploadedimage:1".into(), "../secret.png".into())]),
        );
        assert!(!out.contains("<script>"));
        assert!(!out.contains("<img "));
        assert!(!out.contains("href="));
        assert!(out.contains("default-src 'none'"));
        assert!(out.contains("挿絵未保存"));
    }
    #[test]
    fn bounded_nesting_and_huge_jump() {
        let source = format!(
            "{}word{}[jump:{}]",
            "[b:".repeat(100),
            "]".repeat(100),
            "9".repeat(5000)
        );
        let out = render(&source, "", "", &HashMap::new());
        assert!(out.matches("<strong>").count() <= 16);
        assert!(out.contains("[jump:"));
    }
    #[test]
    fn refs_only_rendered_and_unique() {
        assert_eq!(
            image_refs(
                "[uploadedimage:12][pixivimage:34-2][uploadedimage:12][chapter:[uploadedimage:56]]"
            ),
            vec!["uploadedimage:12", "pixivimage:34-2"]
        );
    }
}
