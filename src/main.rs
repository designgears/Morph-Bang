use anyhow::{anyhow, Context, Result};
use nix::unistd::{chown, Gid, Uid, User};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const WATCH_DIR: &str = "/home";
const LOCK_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
struct Trigger {
    target_ext: String,
    destructive: bool,
}

#[derive(Debug, Clone, Copy)]
struct Owner {
    uid: u32,
    gid: u32,
    mode: u32,
}

impl Owner {
    fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.permissions().mode(),
        }
    }

    fn chown(&self, path: &Path) -> Result<()> {
        chown_path(path, self.uid, self.gid)
    }
}

fn main() -> Result<()> {
    eprintln!(
        "Morph Bang: Global filesystem watch established on {}",
        WATCH_DIR
    );

    let mut child = Command::new("inotifywait")
        .arg("-q")
        .arg("-m")
        .arg("-r")
        .arg("-e")
        .arg("moved_to")
        .arg("--format")
        .arg("%w%f")
        .arg("--exclude")
        .arg("/\\..*")
        .arg(WATCH_DIR)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start inotifywait")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture inotifywait stdout"))?;
    let reader = BufReader::new(stdout);

    let mut locks: HashMap<PathBuf, Instant> = HashMap::new();

    for line in reader.lines() {
        prune_locks(&mut locks);
        let line = match line {
            Ok(v) => v,
            Err(err) => {
                eprintln!("inotify read error: {err}");
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let path = PathBuf::from(line.trim());
        if let Err(err) = handle_path(&path, &mut locks) {
            eprintln!("morph-bang error for {}: {err}", path.display());
        };
    }

    Ok(())
}

fn handle_path(path: &Path, locks: &mut HashMap<PathBuf, Instant>) -> Result<()> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("invalid filename"))?;
    let raw_ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let Some(trigger) = parse_trigger(raw_ext) else {
        return Ok(());
    };
    let clean_path = path.with_extension(&trigger.target_ext);

    if is_locked(locks, &clean_path) {
        return Ok(());
    }
    lock(locks, clean_path.clone());

    if !path.exists() {
        return Ok(());
    }

    let owner = Owner::from_metadata(&fs::metadata(path)?);
    let version_dir = version_dir_for_path(&clean_path, owner.uid)?;
    ensure_version_paths_owned(&version_dir, owner.uid, owner.gid)?;

    if path.is_dir() {
        return handle_directory_trigger(
            path,
            &clean_path,
            filename,
            &trigger,
            &version_dir,
            owner,
        );
    }

    if !path.is_file() {
        return Ok(());
    }

    handle_file_trigger(path, &clean_path, filename, &trigger, &version_dir, owner)
}

fn handle_directory_trigger(
    path: &Path,
    clean_path: &Path,
    filename: &str,
    trigger: &Trigger,
    version_dir: &Path,
    owner: Owner,
) -> Result<()> {
    if trigger.target_ext != "pdf" {
        return Ok(());
    }

    if !trigger.destructive {
        store_directory_version(path, version_dir, owner.uid, owner.gid)?;
    }

    if let Some(existing) = find_latest_version_by_ext(version_dir, &trigger.target_ext) {
        restore_version_file(&existing, clean_path, owner, None)?;
        let _ = fs::remove_dir_all(path);
        notify_restore(owner.uid, filename, &trigger.target_ext);
        return Ok(());
    }

    handle_folder_to_pdf(path, clean_path)?;
    Ok(())
}

fn handle_file_trigger(
    path: &Path,
    clean_path: &Path,
    filename: &str,
    trigger: &Trigger,
    version_dir: &Path,
    owner: Owner,
) -> Result<()> {
    let mime = detect_mime(path)?;
    let source_ext = detect_source_ext(path);
    if !is_valid_target(&mime, &trigger.target_ext) {
        return Ok(());
    }

    if let Some(existing) = find_latest_version_by_ext(version_dir, &trigger.target_ext) {
        if !trigger.destructive {
            store_version(path, version_dir, &source_ext, owner.uid, owner.gid)?;
        }
        restore_version_file(&existing, clean_path, owner, Some(owner.mode))?;
        let _ = fs::remove_file(path);
        notify_restore(owner.uid, filename, &trigger.target_ext);
        return Ok(());
    }

    if !trigger.destructive {
        store_version(path, version_dir, &source_ext, owner.uid, owner.gid)?;
    }

    notify_sync(owner.uid, filename, &trigger.target_ext);

    let temp_file = path.with_extension(format!("morph_tmp.{}", trigger.target_ext));
    let status = morph_engine(path, &temp_file, &trigger.target_ext, &source_ext, &mime)?;
    if status == 0 {
        copy_owner_and_perms(path, &temp_file)?;
        fs::rename(&temp_file, clean_path)?;
        let _ = fs::remove_file(path);
    } else if status == 2 {
        let _ = fs::remove_file(&temp_file);
    }
    let _ = fs::remove_file(&temp_file);
    Ok(())
}

fn notify_restore(uid: u32, filename: &str, target_ext: &str) {
    notify_owner(
        uid,
        &format!(
            "Restored {} from version history ({})",
            filename,
            target_ext.to_uppercase()
        ),
    );
}

fn notify_sync(uid: u32, filename: &str, target_ext: &str) {
    notify_owner(
        uid,
        &format!("Syncing {} to {}", filename, target_ext.to_uppercase()),
    );
}

fn restore_version_file(
    version_file: &Path,
    destination: &Path,
    owner: Owner,
    mode_override: Option<u32>,
) -> Result<()> {
    fs::copy(version_file, destination).with_context(|| {
        format!(
            "failed to restore version {} -> {}",
            version_file.display(),
            destination.display()
        )
    })?;
    owner.chown(destination)?;
    let mode = match mode_override {
        Some(v) => v,
        None => fs::metadata(version_file)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644),
    };
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn handle_folder_to_pdf(input_dir: &Path, output_pdf: &Path) -> Result<()> {
    let owner = Owner::from_metadata(&fs::metadata(input_dir)?);

    let base = input_dir.with_extension("");
    let temp_dir = PathBuf::from(format!("{}.morph_tmp_pdfs", base.display()));
    let final_tmp = PathBuf::from(format!("{}.morph_tmp.pdf", base.display()));
    fs::create_dir_all(&temp_dir)?;

    let files = gather_folder_inputs(input_dir);
    if files.is_empty() {
        let _ = fs::remove_dir_all(&temp_dir);
        return Ok(());
    }

    notify_owner(
        owner.uid,
        &format!("Creating PDF from {} files", files.len()),
    );

    for (idx, file) in files.iter().enumerate() {
        let page = temp_dir.join(format!("{:04}.pdf", idx + 1));
        let mime = detect_mime(file).unwrap_or_default();
        let src_ext = detect_source_ext(file);
        if mime.starts_with("image/") {
            run_cmd(Command::new("magick").arg(file).arg(&page))?;
        } else {
            let from = pandoc_from_ext(&src_ext);
            run_cmd(
                Command::new("pandoc")
                    .arg("-f")
                    .arg(from)
                    .arg(file)
                    .arg("-s")
                    .arg("--pdf-engine=xelatex")
                    .arg("-o")
                    .arg(&page),
            )?;
        }
    }

    let mut pdf_pages: Vec<PathBuf> = WalkDir::new(&temp_dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("pdf"))
        .collect();
    pdf_pages.sort();

    if pdf_pages.is_empty() {
        let _ = fs::remove_dir_all(&temp_dir);
        return Ok(());
    }

    let mut cmd = Command::new("pdfunite");
    for page in &pdf_pages {
        cmd.arg(page);
    }
    cmd.arg(&final_tmp);
    run_cmd(&mut cmd)?;

    chown_path(&final_tmp, owner.uid, owner.gid)?;
    fs::set_permissions(&final_tmp, fs::Permissions::from_mode(0o644)).ok();
    let _ = fs::remove_dir_all(input_dir);
    fs::rename(&final_tmp, output_pdf)?;
    let _ = fs::remove_dir_all(&temp_dir);
    Ok(())
}

fn gather_folder_inputs(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.is_file())
        .filter(|p| is_supported_folder_input(p))
        .collect();
    files.sort();
    files
}

fn morph_engine(
    input: &Path,
    out: &Path,
    target_ext: &str,
    source_ext: &str,
    mime: &str,
) -> Result<i32> {
    if mime.starts_with("image/") || mime == "application/pdf" || mime == "application/postscript" {
        if source_ext == "pdf" {
            let pages = pdf_pages(input).unwrap_or(1);
            if pages > 1 {
                let dir_path = input.with_extension("");
                fs::create_dir_all(&dir_path)?;
                let mut success = false;
                for i in 0..pages {
                    let page_file = dir_path.join(format!("{:03}.{}", i + 1, target_ext));
                    let in_arg = format!("{}[dpi=300,page={}]", input.display(), i);
                    if run_cmd(Command::new("vips").arg("copy").arg(in_arg).arg(&page_file)).is_ok()
                    {
                        copy_owner_and_perms(input, &page_file).ok();
                        success = true;
                    }
                }
                if success {
                    let _ = fs::remove_file(input);
                    return Ok(2);
                }
            }
        }
        if matches!(source_ext, "svg" | "svgz" | "eps" | "ai" | "pdf") {
            let in_arg = format!("{}[dpi=300,scale=2]", input.display());
            if run_cmd(Command::new("vips").arg("copy").arg(in_arg).arg(out)).is_ok() {
                return Ok(0);
            }
        }
        run_cmd(Command::new("vips").arg("copy").arg(input).arg(out))?;
        return Ok(0);
    }

    if mime.starts_with("video/") || mime.starts_with("audio/") {
        if run_cmd(
            Command::new("ffmpeg")
                .arg("-y")
                .arg("-i")
                .arg(input)
                .arg("-c")
                .arg("copy")
                .arg("-map")
                .arg("0")
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("error")
                .arg(out),
        )
        .is_ok()
        {
            return Ok(0);
        }
        run_cmd(
            Command::new("ffmpeg")
                .arg("-y")
                .arg("-i")
                .arg(input)
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("error")
                .arg(out),
        )?;
        return Ok(0);
    }

    if is_doc_output(target_ext) {
        let from = pandoc_from_ext(source_ext);
        let mut cmd = Command::new("pandoc");
        cmd.arg("-f").arg(from).arg(input).arg("-s");
        if target_ext == "pdf" {
            cmd.arg("--pdf-engine=xelatex");
        } else {
            cmd.arg("--mathjax");
        }
        cmd.arg("-o").arg(out);
        run_cmd(&mut cmd)?;
        return Ok(0);
    }

    Err(anyhow!("unsupported conversion"))
}

fn copy_owner_and_perms(src: &Path, dst: &Path) -> Result<()> {
    let meta = fs::metadata(src)?;
    chown(
        dst,
        Some(Uid::from_raw(meta.uid())),
        Some(Gid::from_raw(meta.gid())),
    )
    .ok();
    fs::set_permissions(dst, fs::Permissions::from_mode(meta.permissions().mode()))?;
    Ok(())
}

fn detect_mime(path: &Path) -> Result<String> {
    let out = Command::new("file")
        .arg("--mime-type")
        .arg("-b")
        .arg(path)
        .output()
        .context("file --mime-type failed")?;
    if !out.status.success() {
        return Err(anyhow!("file --mime-type returned non-zero"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn detect_source_ext(path: &Path) -> String {
    let out = Command::new("file")
        .arg("--extension")
        .arg("-b")
        .arg(path)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .split('/')
            .next()
            .unwrap_or("")
            .trim_end_matches('?')
            .to_lowercase(),
        _ => detect_mime(path)
            .ok()
            .map(|m| source_ext_from_mime(&m).to_string())
            .unwrap_or_default(),
    }
}

fn source_ext_from_mime(mime: &str) -> &'static str {
    if mime == "application/pdf" {
        return "pdf";
    }
    if mime.starts_with("image/") {
        return "png";
    }
    if mime.starts_with("video/") {
        return "mp4";
    }
    if mime.starts_with("audio/") {
        return "mp3";
    }
    if mime.contains("officedocument.wordprocessingml.document") {
        return "docx";
    }
    if mime == "application/vnd.oasis.opendocument.text" {
        return "odt";
    }
    if mime.starts_with("application/epub") {
        return "epub";
    }
    if mime == "text/html" {
        return "html";
    }
    if mime.starts_with("text/") {
        return "md";
    }
    if mime == "application/rtf" {
        return "rtf";
    }
    if mime == "application/json" {
        return "json";
    }
    ""
}

fn is_supported_folder_input(path: &Path) -> bool {
    let mime = match detect_mime(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if mime.starts_with("image/") {
        return true;
    }
    let source_ext = detect_source_ext(path);
    is_doc_folder_ext(&source_ext)
}

fn pdf_pages(path: &Path) -> Option<u32> {
    let out = Command::new("pdfinfo").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn notify_owner(uid: u32, body: &str) {
    let username = match uid_to_username(uid) {
        Some(u) => u,
        None => return,
    };
    let bus = format!("unix:path=/run/user/{uid}/bus");
    let _ = Command::new("sudo")
        .arg("-u")
        .arg(username)
        .arg("env")
        .arg(format!("DBUS_SESSION_BUS_ADDRESS={bus}"))
        .arg("notify-send")
        .arg("-a")
        .arg("Morph Bang")
        .arg("-i")
        .arg("document-export")
        .arg("Morphing Data")
        .arg(body)
        .status();
}

fn uid_to_username(uid: u32) -> Option<String> {
    let out = Command::new("id")
        .arg("-nu")
        .arg(uid.to_string())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(anyhow!("command failed"))
        } else {
            Err(anyhow!("command failed: {stderr}"))
        }
    }
}

fn is_locked(locks: &HashMap<PathBuf, Instant>, key: &Path) -> bool {
    locks.get(key).is_some_and(|ts| ts.elapsed() < LOCK_TTL)
}

fn lock(locks: &mut HashMap<PathBuf, Instant>, key: PathBuf) {
    locks.insert(key, Instant::now());
}

fn prune_locks(locks: &mut HashMap<PathBuf, Instant>) {
    locks.retain(|_, ts| ts.elapsed() < LOCK_TTL);
}

fn is_valid_target(mime: &str, ext: &str) -> bool {
    if mime.starts_with("image/") || mime == "application/postscript" || mime == "application/pdf" {
        return is_image_output(ext) || is_doc_output(ext);
    }
    if mime.starts_with("video/") {
        return is_media_output(ext) || is_image_output(ext);
    }
    if mime.starts_with("audio/") {
        return is_media_output(ext);
    }
    if mime.starts_with("text/")
        || mime == "application/pdf"
        || mime.contains("officedocument")
        || mime.starts_with("application/epub")
        || mime == "application/json"
    {
        return is_doc_output(ext);
    }
    false
}

fn is_image_output(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "jpe"
            | "jfif"
            | "webp"
            | "avif"
            | "heic"
            | "heif"
            | "tiff"
            | "tif"
            | "gif"
            | "jxl"
            | "jp2"
            | "j2k"
            | "jpc"
            | "jpt"
            | "j2c"
            | "hdr"
            | "ppm"
            | "pgm"
            | "pbm"
            | "pfm"
            | "pnm"
            | "fits"
            | "fit"
            | "fts"
            | "bmp"
            | "ico"
            | "psd"
            | "tga"
            | "pcx"
            | "pdf"
            | "eps"
            | "dds"
    )
}

fn is_media_output(ext: &str) -> bool {
    matches!(
        ext,
        "mp4"
            | "mkv"
            | "mov"
            | "avi"
            | "mp3"
            | "wav"
            | "flac"
            | "ogg"
            | "m4a"
            | "aac"
            | "webm"
            | "opus"
            | "m4v"
            | "ts"
            | "mts"
            | "flv"
            | "gif"
            | "mpg"
            | "mpeg"
            | "vob"
            | "ogv"
            | "oga"
            | "wv"
            | "ac3"
            | "dts"
            | "aiff"
            | "au"
            | "amr"
            | "3gp"
            | "3g2"
            | "mka"
            | "mxf"
            | "asf"
            | "wmv"
            | "rm"
            | "rmvb"
            | "adts"
            | "spx"
    )
}

fn is_doc_output(ext: &str) -> bool {
    matches!(
        ext,
        "md" | "markdown"
            | "txt"
            | "html"
            | "htm"
            | "docx"
            | "odt"
            | "epub"
            | "latex"
            | "tex"
            | "rst"
            | "rtf"
            | "org"
            | "wiki"
            | "textile"
            | "fb2"
            | "ipynb"
            | "jira"
            | "opml"
            | "json"
            | "typst"
            | "djot"
            | "man"
            | "pdf"
            | "pptx"
            | "beamer"
            | "icml"
            | "tei"
            | "texinfo"
            | "context"
            | "ms"
            | "adoc"
            | "asciidoc"
    )
}

fn is_doc_folder_ext(ext: &str) -> bool {
    matches!(
        ext,
        "md" | "txt"
            | "html"
            | "htm"
            | "docx"
            | "odt"
            | "epub"
            | "tex"
            | "rst"
            | "rtf"
            | "org"
            | "textile"
            | "ipynb"
            | "typst"
    )
}

fn pandoc_from_ext(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "html",
        "docx" => "docx",
        "odt" => "odt",
        "epub" => "epub",
        "latex" | "tex" => "latex",
        "rst" => "rst",
        "rtf" => "rtf",
        "org" => "org",
        "wiki" => "mediawiki",
        "textile" => "textile",
        "fb2" => "fb2",
        "ipynb" => "ipynb",
        "jira" => "jira",
        "opml" => "opml",
        "json" => "json",
        "typst" => "typst",
        "djot" => "djot",
        "csv" => "csv",
        "tsv" => "tsv",
        "t2t" => "t2t",
        "creole" => "creole",
        "twiki" => "twiki",
        "man" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => "man",
        "xml" => "docbook",
        _ => "markdown",
    }
}

fn parse_trigger(raw_ext: &str) -> Option<Trigger> {
    let lower = raw_ext.to_lowercase();
    if lower.starts_with("!!") && lower.len() > 2 {
        return Some(Trigger {
            target_ext: lower.trim_start_matches("!!").to_string(),
            destructive: true,
        });
    }
    if lower.starts_with('!') && lower.len() > 1 {
        return Some(Trigger {
            target_ext: lower.trim_start_matches('!').to_string(),
            destructive: false,
        });
    }
    None
}

fn version_dir_for_path(path: &Path, uid: u32) -> Result<PathBuf> {
    let key = stable_path_key(path, uid);
    let home_dir = home_dir_for_uid(uid)?;
    Ok(home_dir.join(".local/share/morph-bang/versions").join(key))
}

fn ensure_version_paths_owned(version_dir: &Path, uid: u32, gid: u32) -> Result<()> {
    let versions_root = version_dir
        .parent()
        .ok_or_else(|| anyhow!("invalid version directory"))?;
    let app_root = versions_root
        .parent()
        .ok_or_else(|| anyhow!("invalid versions root"))?;

    fs::create_dir_all(app_root).context("failed to create app version root")?;
    fs::create_dir_all(versions_root).context("failed to create versions root")?;
    fs::create_dir_all(version_dir).context("failed to create version directory")?;

    chown_path(app_root, uid, gid)?;
    chown_path(versions_root, uid, gid)?;
    chown_path(version_dir, uid, gid)?;
    Ok(())
}

fn home_dir_for_uid(uid: u32) -> Result<PathBuf> {
    let user = User::from_uid(Uid::from_raw(uid))
        .context("failed to resolve user by uid")?
        .ok_or_else(|| anyhow!("no user entry for uid {}", uid))?;
    Ok(user.dir)
}

fn store_version(
    source_path: &Path,
    version_dir: &Path,
    source_ext: &str,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let ext = sanitize_ext(if source_ext.is_empty() {
        "bin"
    } else {
        source_ext
    });
    let version_file = next_version_path(version_dir, &ext)?;
    fs::copy(source_path, &version_file)?;
    chown_path(&version_file, uid, gid)?;
    Ok(())
}

fn store_directory_version(
    source_dir: &Path,
    version_dir: &Path,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let parent = source_dir
        .parent()
        .ok_or_else(|| anyhow!("directory has no parent: {}", source_dir.display()))?;
    let name = source_dir
        .file_name()
        .ok_or_else(|| anyhow!("directory has no name: {}", source_dir.display()))?;
    let version_file = next_version_path(version_dir, "dir.tar")?;

    run_cmd(
        Command::new("tar")
            .arg("-C")
            .arg(parent)
            .arg("-cf")
            .arg(&version_file)
            .arg(name),
    )?;
    chown_path(&version_file, uid, gid)?;
    Ok(())
}

fn next_version_path(version_dir: &Path, ext: &str) -> Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("clock error")?
        .as_nanos();
    let pid = std::process::id();
    for seq in 0..1024u32 {
        let candidate = version_dir.join(format!("{ts:020}-{pid:05}-{seq:04}.{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "failed to allocate unique version filename in {}",
        version_dir.display()
    ))
}

fn sanitize_ext(ext: &str) -> String {
    let sanitized: String = ext
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "bin".to_string()
    } else {
        sanitized
    }
}

fn stable_path_key(path: &Path, uid: u32) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"morph-bang:v1:path-key");
    hasher.update(&uid.to_be_bytes());
    hasher.update(&[0]);
    hasher.update(path.as_os_str().as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> Result<()> {
    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
        .with_context(|| format!("failed to set ownership on {}", path.display()))?;
    Ok(())
}

fn find_latest_version_by_ext(version_dir: &Path, target_ext: &str) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = fs::read_dir(version_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case(target_ext))
                .unwrap_or(false)
        })
        .collect();
    matches.sort();
    matches.pop()
}
