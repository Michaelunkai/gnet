//! gnet - self-healing .NET installer / repair tool for Windows.
//!
//! Truth comes from the filesystem, never from installer exit codes:
//! a target counts as installed only when its files really exist under a
//! dotnet root. Anything missing is walked up an escalation ladder
//! (winget -> direct installer -> bundle repair -> uninstall+reinstall)
//! until it is verified present. Catalog IDs are discovered from winget at
//! run time, so any future .NET version is handled with no code change.
//! std-only: no third-party crates.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const REPARSE: u32 = 0x400;
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

#[repr(C)]
#[derive(Default)]
struct SysTime {
    y: u16,
    m: u16,
    dow: u16,
    d: u16,
    h: u16,
    mi: u16,
    s: u16,
    ms: u16,
}
extern "system" {
    fn GetLocalTime(t: *mut SysTime);
}

static LOG: OnceLock<Mutex<fs::File>> = OnceLock::new();

fn log(level: &str, msg: &str) {
    let mut t = SysTime::default();
    unsafe { GetLocalTime(&mut t) };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} [{}] {}",
        t.y, t.m, t.d, t.h, t.mi, t.s, level, msg
    );
    println!("{line}");
    if let Some(f) = LOG.get() {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{line}");
        }
    }
}

// ---------------------------------------------------------------- process

struct Out {
    code: i32,
    text: String,
}

fn emit(p: &mut Vec<u8>, buf: &Arc<Mutex<String>>, quiet: bool) {
    let s = String::from_utf8_lossy(p).trim().to_string();
    p.clear();
    if s.is_empty() {
        return;
    }
    let noise = s
        .chars()
        .all(|c| "-\\|/ ".contains(c) || (!c.is_ascii() && !c.is_alphanumeric()));
    if noise || s.contains(" MB /") || s.contains(" KB /") || s.contains(" GB /") {
        return;
    }
    if !quiet {
        log("INFO", &format!("    {s}"));
    }
    if let Ok(mut b) = buf.lock() {
        b.push_str(&s);
        b.push('\n');
    }
}

fn pump<R: Read + Send + 'static>(mut r: R, buf: Arc<Mutex<String>>, quiet: bool) {
    std::thread::spawn(move || {
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in &chunk[..n] {
                        if b == b'\n' || b == b'\r' {
                            emit(&mut pending, &buf, quiet);
                        } else {
                            pending.push(b);
                        }
                    }
                }
            }
        }
        emit(&mut pending, &buf, quiet);
    });
}

fn run(exe: &str, args: &[&str], timeout_s: u64, label: &str, quiet: bool) -> Out {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log("WARN", &format!("{label}: cannot start {exe}: {e}"));
            return Out { code: -1, text: String::new() };
        }
    };
    let buf = Arc::new(Mutex::new(String::new()));
    if let Some(o) = child.stdout.take() {
        pump(o, buf.clone(), quiet);
    }
    if let Some(e) = child.stderr.take() {
        pump(e, buf.clone(), quiet);
    }
    let start = Instant::now();
    let mut beat = 60u64;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) => {}
            Err(_) => break -1,
        }
        let el = start.elapsed().as_secs();
        if el >= timeout_s {
            log("WARN", &format!("{label}: timed out after {timeout_s}s, terminating"));
            let _ = child.kill();
            let _ = child.wait();
            break -2;
        }
        if el >= beat {
            beat = el + 60;
            log("INFO", &format!("    {label}: still running ({el}s of {timeout_s}s)"));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    std::thread::sleep(Duration::from_millis(400));
    let text = buf.lock().map(|b| b.clone()).unwrap_or_default();
    Out { code, text }
}

/// Like `run`, but waits and retries while another installer owns the
/// Windows Installer mutex (exit 1618).
fn run_retry(exe: &str, args: &[&str], timeout_s: u64, label: &str) -> Out {
    let mut o = run(exe, args, timeout_s, label, false);
    for _ in 0..6 {
        if o.code != 1618 {
            break;
        }
        log("WARN", &format!("{label}: another installation is in progress (1618), waiting 30s"));
        std::thread::sleep(Duration::from_secs(30));
        o = run(exe, args, timeout_s, label, false);
    }
    o
}

// ---------------------------------------------------------------- environment

fn is_admin() -> bool {
    run("fltmc", &[], 20, "admin check", true).code == 0
}

fn reg_query(args: &[&str]) -> Out {
    let mut a = vec!["query"];
    a.extend_from_slice(args);
    run("reg", &a, 60, "reg", true)
}

fn pf() -> PathBuf {
    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()))
}

fn pf86() -> PathBuf {
    PathBuf::from(std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into()))
}

fn is_reparse(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_attributes() & REPARSE != 0)
        .unwrap_or(false)
}

fn rmdir_link(p: &Path) -> bool {
    run("cmd", &["/c", "rmdir", &p.to_string_lossy()], 30, "rmdir", true).code == 0
}

/// Dangling junctions/symlinks make every installer die with path-not-found
/// (exit 3 / 1603) while the registry still claims the runtime exists.
fn repair_roots() {
    for root in [pf().join("dotnet"), pf86().join("dotnet")] {
        if is_reparse(&root) && fs::metadata(&root).is_err() {
            let tgt = fs::read_link(&root).map(|t| t.display().to_string()).unwrap_or_default();
            log("WARN", &format!("install root {} is a dangling link to '{}' - replacing with a real folder", root.display(), tgt));
            if !rmdir_link(&root) {
                log("ERROR", &format!("could not remove dangling link {}", root.display()));
            }
        }
        if !root.exists() {
            match fs::create_dir_all(&root) {
                Ok(_) => log("OK", &format!("install root {} recreated", root.display())),
                Err(e) => log("ERROR", &format!("cannot create {}: {e}", root.display())),
            }
        }
        // dangling links one or two levels below the root (shared\X, sdk, host, packs...)
        sweep_dangling(&root, 3);
    }
}

fn sweep_dangling(dir: &Path, depth: u32) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if is_reparse(&p) {
            if fs::metadata(&p).is_err() {
                log("WARN", &format!("removing dangling link {}", p.display()));
                if !rmdir_link(&p) {
                    let _ = fs::remove_file(&p);
                }
            }
        } else if p.is_dir() {
            sweep_dangling(&p, depth - 1);
        }
    }
}

/// DOTNET_ROOT pointing at a folder that does not exist breaks every host.
fn doctor_env() {
    for scope in ["HKCU\\Environment", "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"] {
        for var in ["DOTNET_ROOT", "DOTNET_ROOT(x86)", "DOTNET_INSTALL_DIR"] {
            let o = reg_query(&[scope, "/v", var]);
            if o.code != 0 {
                continue;
            }
            let val = o.text.lines().find(|l| l.contains(var)).and_then(|l| l.split("REG_").nth(1)).map(|r| r.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim().to_string()).unwrap_or_default();
            if !val.is_empty() && !Path::new(&val).exists() {
                log("WARN", &format!("{scope}\\{var} = '{val}' does not exist - removing"));
                let _ = run("reg", &["delete", scope, "/v", var, "/f"], 30, "reg delete", true);
            }
        }
    }
}

fn dotnet_roots() -> Vec<PathBuf> {
    let mut r: Vec<PathBuf> = vec![pf().join("dotnet")];
    if let Ok(v) = std::env::var("DOTNET_ROOT") {
        r.push(PathBuf::from(v));
    }
    for scope_var in ["HKCU\\Environment", "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"] {
        let o = reg_query(&[scope_var, "/v", "DOTNET_ROOT"]);
        if o.code == 0 {
            if let Some(l) = o.text.lines().find(|l| l.contains("DOTNET_ROOT")) {
                if let Some(p) = l.split("REG_SZ").nth(1).or_else(|| l.split("REG_EXPAND_SZ").nth(1)) {
                    r.push(PathBuf::from(p.trim()));
                }
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for d in path.split(';') {
            let d = d.trim();
            if !d.is_empty() && Path::new(d).join("dotnet.exe").is_file() {
                r.push(PathBuf::from(d));
            }
        }
    }
    let mut seen = BTreeSet::new();
    r.retain(|p| {
        let k = p.to_string_lossy().to_lowercase();
        !k.contains("(x86)") && p.is_dir() && seen.insert(k)
    });
    r
}

/// Everything really present on disk: "fw/Microsoft.NETCore.App/3.1.32", "sdk/3.1.426".
fn installed_set() -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    for root in dotnet_roots() {
        if let Ok(rd) = fs::read_dir(root.join("shared")) {
            for fw in rd.flatten() {
                if let Ok(vs) = fs::read_dir(fw.path()) {
                    for v in vs.flatten() {
                        if non_empty(&v.path()) {
                            s.insert(format!("fw/{}/{}", fw.file_name().to_string_lossy(), v.file_name().to_string_lossy()));
                        }
                    }
                }
            }
        }
        if let Ok(rd) = fs::read_dir(root.join("sdk")) {
            for v in rd.flatten() {
                if non_empty(&v.path()) && v.path().is_dir() {
                    s.insert(format!("sdk/{}", v.file_name().to_string_lossy()));
                }
            }
        }
    }
    s
}

fn non_empty(p: &Path) -> bool {
    fs::read_dir(p).map(|mut r| r.next().is_some()).unwrap_or(false)
}

// ---------------------------------------------------------------- catalog

#[derive(Clone, Debug)]
struct Target {
    id: String,
    family: String,
    tail: String,
    name: String,
    version: String,
    url: String,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Core,
    Desktop,
    Asp,
    Sdk,
    Hosting,
    Framework,
}

fn fw_name(k: Kind) -> &'static str {
    match k {
        Kind::Core => "Microsoft.NETCore.App",
        Kind::Desktop => "Microsoft.WindowsDesktop.App",
        Kind::Asp | Kind::Hosting => "Microsoft.AspNetCore.App",
        _ => "",
    }
}

fn parse_id(id: &str) -> Option<(String, String)> {
    let rest = id.strip_prefix("Microsoft.DotNet.")?;
    let (fam, tail) = rest.split_once('.')?;
    if !["Runtime", "DesktopRuntime", "AspNetCore", "SDK", "HostingBundle", "Framework"].contains(&fam) {
        return None;
    }
    if fam == "Framework" {
        return Some((fam.into(), tail.into()));
    }
    // every segment of the tail must be numeric: skips Preview / Insiders / RC ids
    let ok = !tail.is_empty() && tail.replace('_', ".").split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if ok {
        Some((fam.into(), tail.into()))
    } else {
        None
    }
}

fn catalog_ids(winget: &str) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for q in ["Microsoft.DotNet.Runtime", "Microsoft.DotNet.DesktopRuntime", "Microsoft.DotNet.AspNetCore", "Microsoft.DotNet.SDK", "Microsoft.DotNet.HostingBundle", "Microsoft.DotNet.Framework"] {
        for attempt in 0..3 {
            let o = run(winget, &["search", "--query", q, "--source", "winget", "--count", "500", "--accept-source-agreements", "--disable-interactivity"], 180, "catalog", true);
            let before = ids.len();
            for tok in o.text.split_whitespace() {
                if tok.starts_with("Microsoft.DotNet.") && parse_id(tok).is_some() {
                    ids.insert(tok.to_string());
                }
            }
            if o.code == 0 || ids.len() > before || attempt == 2 {
                break;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    // a Framework package set that is "Runtime" only: always keep the known anchors too
    ids.insert("Microsoft.DotNet.Framework.DeveloperPack_4".into());
    ids.into_iter().collect()
}

fn kind_of(family: &str, name: &str) -> Kind {
    if family == "Framework" {
        return Kind::Framework;
    }
    let n = name.to_lowercase();
    if n.contains("hosting bundle") {
        Kind::Hosting
    } else if n.contains("windows desktop") {
        Kind::Desktop
    } else if n.contains("asp.net core") && n.contains("runtime") {
        Kind::Asp
    } else if n.contains(" sdk") {
        Kind::Sdk
    } else if n.contains("runtime") {
        Kind::Core
    } else {
        match family {
            "DesktopRuntime" => Kind::Desktop,
            "AspNetCore" => Kind::Asp,
            "SDK" => Kind::Sdk,
            "HostingBundle" => Kind::Hosting,
            _ => Kind::Core,
        }
    }
}

fn show(winget: &str, id: &str) -> Option<Target> {
    let (family, tail) = parse_id(id)?;
    let mut name = String::new();
    let mut version = String::new();
    let mut url = String::new();
    for _ in 0..3 {
        let o = run(winget, &["show", "--id", id, "--exact", "--source", "winget", "--architecture", "x64", "--accept-source-agreements", "--disable-interactivity"], 120, "show", true);
        for l in o.text.lines() {
            let l = l.trim();
            if let Some(r) = l.strip_prefix("Found ") {
                if let Some((n, v)) = r.split_once(" [") {
                    name = n.trim().to_string();
                    version = v.split("] Version ").nth(1).unwrap_or("").trim().to_string();
                }
            }
            if url.is_empty() {
                if let Some(u) = l.strip_prefix("Installer Url:") {
                    url = u.trim().to_string();
                }
            }
        }
        if !name.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let kind = kind_of(&family, &name);
    Some(Target { id: id.into(), family, tail, name, version, url, kind })
}

fn build_plan(winget: &str, ids: &[String]) -> Vec<Target> {
    let out: Arc<Mutex<Vec<Target>>> = Arc::new(Mutex::new(Vec::new()));
    for chunk in ids.chunks(6) {
        std::thread::scope(|sc| {
            for id in chunk {
                let out = out.clone();
                sc.spawn(move || {
                    if let Some(t) = show(winget, id) {
                        out.lock().unwrap().push(t);
                    }
                });
            }
        });
    }
    let mut v = out.lock().unwrap().clone();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

// ---------------------------------------------------------------- presence

fn want_prefix(t: &Target) -> String {
    let p: Vec<String> = t.tail.replace('_', ".").split('.').map(|s| s.to_string()).collect();
    if p.len() >= 2 {
        format!("{}.{}.", p[0], p[1])
    } else {
        format!("{}.0.", p[0])
    }
}

fn present(t: &Target, set: &BTreeSet<String>) -> bool {
    let pre = want_prefix(t);
    match t.kind {
        Kind::Sdk => set.iter().any(|e| e.strip_prefix("sdk/").map(|v| v.starts_with(&pre)).unwrap_or(false)),
        Kind::Framework => framework_present(t),
        k => {
            let fw = format!("fw/{}/{}", fw_name(k), pre);
            set.iter().any(|e| e.starts_with(&fw))
        }
    }
}

fn arp_names() -> Vec<String> {
    let mut names = Vec::new();
    for key in [r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall", r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"] {
        let o = reg_query(&[key, "/s", "/v", "DisplayName"]);
        for l in o.text.lines() {
            if let Some(v) = l.split("REG_SZ").nth(1) {
                names.push(v.trim().to_string());
            }
        }
    }
    names
}

fn framework_present(t: &Target) -> bool {
    if t.id.ends_with("Framework.Runtime") {
        let o = reg_query(&[r"HKLM\SOFTWARE\Microsoft\NET Framework Setup\NDP\v4\Full", "/v", "Release"]);
        return o
            .text
            .split_whitespace()
            .last()
            .and_then(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok())
            .map(|r| r >= 528040)
            .unwrap_or(false);
    }
    let digits: Vec<&str> = t.tail.split(|c| c == '.' || c == '_').filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())).collect();
    let names = arp_names();
    let fam = if digits.len() >= 2 {
        format!("Microsoft .NET Framework {}.{}", digits[0], digits[1])
    } else {
        "Microsoft .NET Framework 4.8.1 ".to_string()
    };
    names.iter().any(|n| n.starts_with(&fam) && (n.contains("Developer Pack") || n.contains("Targeting Pack") || n.contains("Multi-Targeting") || n.contains("SDK")))
}

fn iis_enabled() -> bool {
    reg_query(&[r"HKLM\SOFTWARE\Microsoft\InetStp", "/v", "MajorVersion"]).code == 0
}

// ---------------------------------------------------------------- install

struct Cfg {
    winget: Option<String>,
    timeout: u64,
    dl_dir: PathBuf,
    no_winget: bool,
    reboot: AtomicBool,
}

fn download(url: &str, dest: &Path) -> bool {
    let _ = fs::create_dir_all(dest.parent().unwrap_or(Path::new(".")));
    let _ = fs::remove_file(dest);
    for attempt in 1..=3 {
        let o = run("curl.exe", &["-L", "--fail", "--silent", "--show-error", "--retry", "3", "-A", UA, "-o", &dest.to_string_lossy(), url], 1800, "download", true);
        if o.code == 0 && fs::metadata(dest).map(|m| m.len() > 100_000).unwrap_or(false) {
            return true;
        }
        log("WARN", &format!("download attempt {attempt} failed (exit {}): {}", o.code, o.text.trim()));
        std::thread::sleep(Duration::from_secs(3));
    }
    false
}

fn note_code(cfg: &Cfg, code: i32) {
    if code == 3010 || code == 1641 {
        cfg.reboot.store(true, Ordering::SeqCst);
    }
}

fn installer_failed(text: &str) -> bool {
    text.lines().any(|l| {
        l.split("Installer failed with exit code:")
            .nth(1)
            .map(|c| !matches!(c.trim().parse::<i64>().unwrap_or(1), 0 | 3010 | 1641))
            .unwrap_or(false)
    })
}

fn heal(cfg: &Cfg, t: &Target) -> bool {
    let check = || present(t, &installed_set());
    if check() {
        return true;
    }
    // 1. winget force install
    if let (Some(w), false) = (&cfg.winget, cfg.no_winget) {
        let o = run_retry(w, &["install", "--id", &t.id, "--exact", "--source", "winget", "--architecture", "x64", "--force", "--silent", "--accept-package-agreements", "--accept-source-agreements", "--disable-interactivity"], cfg.timeout, &t.id);
        note_code(cfg, o.code);
        if installer_failed(&o.text) {
            log("WARN", &format!("{} : winget reported an inner installer failure", t.id));
        }
        std::thread::sleep(Duration::from_secs(3));
        if check() {
            return true;
        }
    }
    // 2..4 direct installer ladder
    if t.url.is_empty() {
        log("WARN", &format!("{} : no installer url in the catalog", t.id));
        return false;
    }
    let leaf = t.url.rsplit('/').next().unwrap_or("installer.exe");
    let dest = cfg.dl_dir.join(format!("{}-{}", t.id.replace(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-', "_"), leaf));
    if !download(&t.url, &dest) {
        return false;
    }
    let d = dest.to_string_lossy().to_string();
    let is_msi = leaf.to_lowercase().ends_with(".msi");
    let steps: Vec<(&str, Vec<&str>)> = if is_msi {
        vec![
            ("msi install", vec!["/i", &d, "/qn", "/norestart"]),
            ("msi repair", vec!["/fvomus", &d, "/qn", "/norestart"]),
        ]
    } else {
        vec![
            ("bundle install", vec!["/install", "/quiet", "/norestart"]),
            ("bundle repair", vec!["/repair", "/quiet", "/norestart"]),
            ("bundle uninstall", vec!["/uninstall", "/quiet", "/norestart"]),
            ("bundle reinstall", vec!["/install", "/quiet", "/norestart"]),
        ]
    };
    for (label, args) in steps {
        log("INFO", &format!("    {label}: {}", t.id));
        let exe = if is_msi { "msiexec.exe" } else { d.as_str() };
        let o = run_retry(exe, &args, cfg.timeout, &format!("{label} {}", t.id));
        note_code(cfg, o.code);
        std::thread::sleep(Duration::from_secs(3));
        if check() {
            return true;
        }
        log("WARN", &format!("{label} {} : exit {} but files are still missing", t.id, o.code));
    }
    let _ = fs::remove_file(&dest); // never reuse a possibly corrupt download
    check()
}

// ---------------------------------------------------------------- winget discovery

fn find_winget() -> Option<String> {
    if run("winget", &["--version"], 30, "winget", true).code == 0 {
        return Some("winget".into());
    }
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        let p = Path::new(&la).join(r"Microsoft\WindowsApps\winget.exe");
        if p.is_file() {
            return Some(p.to_string_lossy().into());
        }
    }
    if let Ok(rd) = fs::read_dir(pf().join("WindowsApps")) {
        let mut c: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("Microsoft.DesktopAppInstaller_") && e.file_name().to_string_lossy().contains("x64"))
            .map(|e| e.path().join("winget.exe"))
            .filter(|p| p.is_file())
            .collect();
        c.sort();
        if let Some(p) = c.pop() {
            return Some(p.to_string_lossy().into());
        }
    }
    None
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users\Public".into())).join(r"Documents\WindowsPowerShell\Gnet")
}

fn stamp() -> String {
    let mut t = SysTime::default();
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", t.y, t.m, t.d, t.h, t.mi, t.s)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a.eq_ignore_ascii_case(f));
    if has("--help") || has("-h") {
        println!("gnet {} - self-healing .NET installer\n\nUSAGE: gnet [--scan-only] [--force] [--no-winget] [--no-elevate] [--ids a,b] [--timeout SECS]\n\n  --scan-only   report what is missing, change nothing\n  --force       reinstall every target even if healthy\n  --ids         only these winget ids (comma separated)\n", env!("CARGO_PKG_VERSION"));
        return;
    }
    let scan_only = has("--scan-only") || has("-scanonly");
    let force = has("--force") || has("-force");
    let timeout: u64 = args.iter().position(|a| a == "--timeout").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(1800);
    let only: Option<Vec<String>> = args.iter().position(|a| a == "--ids").and_then(|i| args.get(i + 1)).map(|v| v.split(',').map(|s| s.trim().to_string()).collect());

    let home = home_dir();
    let reports = home.join("Reports");
    let _ = fs::create_dir_all(&reports);
    let report = reports.join(format!("gnet-{}.log", stamp()));
    if let Ok(f) = fs::File::create(&report) {
        let _ = LOG.set(Mutex::new(f));
    }
    println!("GNET report: {}\n", report.display());

    let admin = is_admin();
    log("INFO", &format!("gnet {} (rust)   admin: {admin}", env!("CARGO_PKG_VERSION")));
    if !admin && !scan_only && !has("--no-elevate") {
        log("INFO", "not elevated - relaunching as administrator");
        let exe = std::env::current_exe().unwrap_or_default();
        let argl = args.iter().map(|a| format!("'{}'", a.replace('\'', "''"))).collect::<Vec<_>>().join(",");
        let ps = format!(
            "$p = Start-Process -FilePath '{}' -ArgumentList @({}'--no-elevate') -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
            exe.display(),
            if argl.is_empty() { String::new() } else { format!("{argl},") }
        );
        let st = Command::new("powershell.exe").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps]).status();
        std::process::exit(st.ok().and_then(|s| s.code()).unwrap_or(1));
    }

    // never let an installer restart the machine: abort any pending shutdown
    let stop = Arc::new(AtomicBool::new(false));
    let guard = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let _ = Command::new("shutdown").arg("/a").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).status();
                std::thread::sleep(Duration::from_secs(2));
            }
        })
    };

    let code = real_main(scan_only, force, timeout, only, home.join("downloads"), has("--no-winget"));
    stop.store(true, Ordering::SeqCst);
    let _ = guard.join();
    std::process::exit(code);
}

fn real_main(scan_only: bool, force: bool, timeout: u64, only: Option<Vec<String>>, dl_dir: PathBuf, no_winget: bool) -> i32 {
    if !scan_only {
        repair_roots();
        doctor_env();
    }
    let winget = find_winget();
    match &winget {
        Some(w) => log("INFO", &format!("winget: {w}")),
        None => {
            log("ERROR", "winget is unavailable - cannot resolve the .NET catalog");
            return 1;
        }
    }
    let w = winget.clone().unwrap();
    let mut ids = catalog_ids(&w);
    if let Some(o) = &only {
        ids.retain(|i| o.contains(i));
    }
    log("INFO", &format!("catalog: {} .NET package ids discovered", ids.len()));
    let plan = build_plan(&w, &ids);

    let cfg = Cfg { winget, timeout, dl_dir, no_winget, reboot: AtomicBool::new(false) };
    let iis = iis_enabled();
    let mut ok = 0;
    let mut failed: Vec<String> = Vec::new();
    let mut skipped = 0;

    let mut work: Vec<&Target> = Vec::new();
    let set = installed_set();
    for t in &plan {
        if t.kind == Kind::Framework && t.id.contains("Redistributable.1_1") {
            log("WARN", &format!("{} : SKIPPED - .NET Framework 1.1 cannot be installed on 64-bit Windows", t.id));
            skipped += 1;
        } else if t.kind == Kind::Hosting && !iis {
            log("WARN", &format!("{} : SKIPPED - the IIS web server feature is not enabled", t.id));
            skipped += 1;
        } else if !force && present(t, &set) {
            log("OK", &format!("{} : already installed and working - skipped", t.id));
            ok += 1;
        } else {
            log("WARN", &format!("{} : {} {}missing or broken", t.id, t.name, if t.version.is_empty() { String::new() } else { format!("({}) ", t.version) }));
            work.push(t);
        }
    }
    log("INFO", &format!("health check: {} of {} target(s) already working, {} need attention", ok, plan.len() - skipped, work.len()));

    if scan_only {
        log("INFO", "scan-only: no changes made");
        return if work.is_empty() { 0 } else { 1 };
    }

    for pass in 1..=3 {
        if work.is_empty() {
            break;
        }
        log("STEP", &format!("pass {pass}: {} target(s) to heal", work.len()));
        let mut still: Vec<&Target> = Vec::new();
        for (i, t) in work.iter().enumerate() {
            log("STEP", &format!("[{}/{}] {}", i + 1, work.len(), t.id));
            if heal(&cfg, t) {
                log("OK", &format!("{} : verified present on disk", t.id));
                ok += 1;
            } else {
                log("WARN", &format!("{} : still not present after pass {pass}", t.id));
                still.push(t);
            }
        }
        work = still;
        if !work.is_empty() {
            repair_roots();
            std::thread::sleep(Duration::from_secs(5));
        }
    }
    for t in &work {
        failed.push(t.id.clone());
        log("ERROR", &format!("{} : FAILED after all repair strategies", t.id));
    }

    // final truth: what the real dotnet host reports
    for h in dotnet_roots() {
        let exe = h.join("dotnet.exe");
        if exe.is_file() {
            let e = exe.to_string_lossy().to_string();
            log("INFO", &format!("dotnet host: {e}"));
            for k in ["--list-sdks", "--list-runtimes"] {
                let o = run(&e, &[k], 60, "dotnet", true);
                for l in o.text.lines() {
                    log("INFO", &format!("    {l}"));
                }
            }
        }
    }
    log("INFO", &format!("targets: {}   verified: {}   failed: {}   skipped: {}", plan.len(), ok, failed.len(), skipped));
    if !failed.is_empty() {
        return 1;
    }
    if cfg.reboot.load(Ordering::SeqCst) {
        log("WARN", "a reboot is required to finish (not performed automatically)");
        return 3010;
    }
    log("OK", "every .NET target is installed and verified on disk");
    0
}
