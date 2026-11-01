"""Offline subprocess checks for opixiv's staged self-update, including Windows.

Usage: python tools/check_update.py target/release/opixiv[.exe] [--out evidence.json]
A native fake Cargo is put on child PATH. No Git/Pixiv requests or real install.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
CARGO_SOURCE = r'''
use std::{env,fs,path::PathBuf,process};
fn main(){
 let args:Vec<_>=env::args().skip(1).collect();
 if args==["--version"] {println!("cargo 0.0.0-test");return;}
 fs::write(env::var_os("UPDATE_TEST_LOG").unwrap(),args.join("\n")).unwrap();
 assert_eq!(&args[..11],["install","--git","https://github.com/ELRdn/Open-Pixiv-Downloader.git","--branch","main","--locked","--force","--bin","opixiv","--root",args[10].as_str()]);
 let mode=env::var("UPDATE_TEST_MODE").unwrap();
 if mode=="failure" {process::exit(19);}
 if mode=="missing" {return;}
 let root=PathBuf::from(&args[10]);fs::create_dir_all(root.join("bin")).unwrap();
 fs::copy(env::var_os("UPDATE_TEST_SOURCE").unwrap(),root.join("bin").join(if cfg!(windows){"opixiv.exe"}else{"opixiv"})).unwrap();
}
'''
CANDIDATE_SOURCE = r'''
fn main(){
 if std::env::args().nth(1).as_deref()==Some("--version") {
  let version=std::env::var("UPDATE_TEST_VERSION").unwrap();
  if version=="hang" {std::thread::sleep(std::time::Duration::from_secs(30));}
  else if version=="large" {println!("{}","x".repeat(10000));}
  else {println!("{}",version);}
 }else{println!("updated-fixture");}
}
'''


def main() -> None:
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary",type=Path)
    parser.add_argument("--out",type=Path)
    args=parser.parse_args()
    binary=args.binary.resolve()
    suffix=".exe" if os.name=="nt" else ""
    evidence={"platform":platform.system(),"scope":"Offline native fake Cargo + actual self-replacement in disposable subprocesses; no remote downloads","cases":[]}
    with tempfile.TemporaryDirectory(prefix="opixiv-update-check-",ignore_cleanup_errors=True) as temporary:
        work=Path(temporary)/"日本語 space & $"
        helper=work/"helper";helper.mkdir(parents=True)
        candidate=work/("candidate"+suffix)
        for name,source,target in [("cargo",CARGO_SOURCE,helper/("cargo"+suffix)),("candidate",CANDIDATE_SOURCE,candidate)]:
            src=work/(name+".rs");src.write_text(source,encoding="utf-8")
            subprocess.run(["rustc","--edition=2021",str(src),"-o",str(target)],check=True,cwd=ROOT,capture_output=True)
        rust_temp=work/"staging 日本語 & $";rust_temp.mkdir()
        for label,mode,version,expected_success in [
            ("success","copy","opixiv 99.0.0",True),
            ("cargo_failure_keeps_old_binary","failure","opixiv 99.0.0",False),
            ("missing_binary_keeps_old_binary","missing","opixiv 99.0.0",False),
            ("wrong_product_keeps_old_binary","copy","other 99.0.0",False),
            ("downgrade_keeps_old_binary","copy","opixiv 0.1.0",False),
            ("hanging_candidate_keeps_old_binary","copy","hang",False),
            ("oversized_version_keeps_old_binary","copy","large",False),
        ]:
            install=work/label/"bin";install.mkdir(parents=True)
            current=install/("opixiv"+suffix);shutil.copy2(binary,current)
            original_hash=hashlib.sha256(current.read_bytes()).hexdigest()
            archive=work/label/"archive";archive.mkdir();marker=archive/"my-novel.txt";marker.write_bytes(b"preserve local user archive")
            log=work/(label+".args")
            env={**os.environ,"PATH":str(helper)+os.pathsep+os.environ["PATH"],"UPDATE_TEST_MODE":mode,"UPDATE_TEST_VERSION":version,"UPDATE_TEST_SOURCE":str(candidate),"UPDATE_TEST_LOG":str(log),"TMPDIR":str(rust_temp),"TMP":str(rust_temp),"TEMP":str(rust_temp)}
            result=subprocess.run([str(current),"update","--force"],env=env,cwd=work,capture_output=True,text=True,encoding="utf-8",timeout=60)
            assert (result.returncode==0)==expected_success,(label,result.stdout,result.stderr)
            tokens=log.read_text(encoding="utf-8").splitlines()
            assert len(tokens)==11 and tokens[-2]=="--root",tokens
            assert "staging 日本語 & $" in tokens[-1],tokens
            assert marker.read_bytes()==b"preserve local user archive"
            if expected_success:
                installed=subprocess.run([str(current)],env=env,capture_output=True,text=True,encoding="utf-8",timeout=10)
                assert installed.returncode==0 and installed.stdout.strip()=="updated-fixture",installed
                assert hashlib.sha256(current.read_bytes()).hexdigest()!=original_hash
            else:
                assert hashlib.sha256(current.read_bytes()).hexdigest()==original_hash,label
            evidence["cases"].append({"case":label,"status":"passed","exit_code":result.returncode,"archive_untouched":True,"exact_executable_path":True})
    evidence["status"]="passed"
    text=json.dumps(evidence,ensure_ascii=False,indent=2)+"\n"
    if args.out:
        args.out.parent.mkdir(parents=True,exist_ok=True);args.out.write_text(text,encoding="utf-8")
    print(text,end="")


if __name__=="__main__":main()
