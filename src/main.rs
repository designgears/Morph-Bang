mod convertx;
mod formats;

use anyhow::{anyhow, Context, Result};
use nix::unistd::{chown, Gid, Uid, User};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const WATCH_DIR: &str = "/home";
const LOCK_TTL: Duration = Duration::from_secs(2);
const PDF_OUTPUT_PAGE_SIZE: &str = "letter";
const PDF_OUTPUT_PROFILE: &str = "/ebook";
const DOCKER_CONTAINER: &str = "morph-bang-engine";
const DEFAULT_DOCKER_IMAGE: &str = "ghcr.io/c4illin/convertx:latest";

/// How conversion tools (vips, ffmpeg, pandoc, ...) are executed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Engine {
    /// Run binaries installed on the host.
    Native,
    /// Run binaries inside a persistent toolbox container via `docker exec`.
    Docker { image: String },
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| match std::env::var("MORPH_ENGINE").as_deref() {
        Ok("docker") => Engine::Docker {
            image: std::env::var("MORPH_DOCKER_IMAGE")
                .unwrap_or_else(|_| DEFAULT_DOCKER_IMAGE.to_string()),
        },
        _ => Engine::Native,
    })
}

/// Build a command for a conversion tool, routed through the active engine.
/// Paths are passed through unchanged: the toolbox container bind-mounts
/// `/home` and the temp dir at the same locations as the host.
pub(crate) fn tool(name: &str) -> Command {
    match engine() {
        Engine::Native => Command::new(name),
        Engine::Docker { .. } => {
            let mut cmd = Command::new("docker");
            cmd.arg("exec").arg(DOCKER_CONTAINER).arg(name);
            cmd
        }
    }
}

/// Ensure the toolbox container exists and is running. Called once at startup;
/// the container is idle (`sleep infinity`) and reused for every conversion.
fn ensure_engine_ready() -> Result<()> {
    let Engine::Docker { image } = engine() else {
        return Ok(());
    };

    let state = Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}|{{.Config.Image}}", DOCKER_CONTAINER])
        .output()
        .context("failed to run docker; is it installed and running?")?;

    if state.status.success() {
        let s = String::from_utf8_lossy(&state.stdout);
        let s = s.trim();
        let (running, current_image) = s.split_once('|').unwrap_or((s, ""));
        if current_image != image {
            eprintln!("Morph Bang: toolbox image changed ({current_image} -> {image}), recreating");
            run_cmd(Command::new("docker").args(["rm", "-f", DOCKER_CONTAINER]))?;
        } else if running == "true" {
            eprintln!("Morph Bang: reusing toolbox container {DOCKER_CONTAINER} ({image})");
            return Ok(());
        } else {
            eprintln!("Morph Bang: starting toolbox container {DOCKER_CONTAINER}");
            run_cmd(Command::new("docker").args(["start", DOCKER_CONTAINER]))?;
            return Ok(());
        }
    }

    let tmp = std::env::temp_dir();
    let tmp = tmp.to_string_lossy();
    eprintln!("Morph Bang: creating toolbox container {DOCKER_CONTAINER} from {image}");
    run_cmd(
        Command::new("docker")
            .arg("run")
            .arg("-d")
            .arg("--name")
            .arg(DOCKER_CONTAINER)
            .arg("--restart")
            .arg("unless-stopped")
            .arg("-v")
            .arg(format!("{WATCH_DIR}:{WATCH_DIR}"))
            .arg("-v")
            .arg(format!("{tmp}:{tmp}"))
            .arg("--entrypoint")
            .arg("sleep")
            .arg(image)
            .arg("infinity"),
    )
    .context("failed to create toolbox container")?;
    Ok(())
}

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

    fn from_path(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path)?;
        let mut owner = Self::from_metadata(&meta);

        if owner.uid != 0 {
            return Ok(owner);
        }

        if let Some((uid, gid)) = owner_from_home_path(path) {
            owner.uid = uid;
            owner.gid = gid;
            return Ok(owner);
        }

        if let Some(parent) = path.parent() {
            if let Ok(parent_meta) = fs::metadata(parent) {
                if parent_meta.uid() != 0 {
                    owner.uid = parent_meta.uid();
                    owner.gid = parent_meta.gid();
                }
            }
        }

        Ok(owner)
    }

    fn chown(&self, path: &Path) -> Result<()> {
        chown_path(path, self.uid, self.gid)
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("convert") {
        // One-shot mode: process the given trigger paths and exit.
        // Useful for testing engines without the inotify watcher.
        ensure_engine_ready()?;
        let mut locks: HashMap<PathBuf, Instant> = HashMap::new();
        let mut failed = false;
        for p in &args[2..] {
            let path = PathBuf::from(p);
            locks.clear();
            if let Err(err) = handle_path(&path, &mut locks) {
                eprintln!("morph-bang error for {}: {err}", path.display());
                failed = true;
            }
        }
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }

    eprintln!(
        "Morph Bang: Global filesystem watch established on {}",
        WATCH_DIR
    );

    ensure_engine_ready()?;

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
    let Some((stem, trigger)) = parse_trigger_name(filename) else {
        return Ok(());
    };
    let clean_path = path.with_file_name(format!(
        "{stem}.{}",
        convertx::output_ext(&trigger.target_ext)
    ));

    if is_locked(locks, &clean_path) {
        return Ok(());
    }
    lock(locks, clean_path.clone());

    if !path.exists() {
        return Ok(());
    }

    let owner = Owner::from_path(path)?;

    if path.is_dir() {
        return handle_directory_trigger(path, &clean_path, filename, &trigger, owner);
    }

    if !path.is_file() {
        return Ok(());
    }

    let version_dir = version_dir_for_path(&clean_path, owner.uid)?;
    ensure_version_paths_owned(&version_dir, owner.uid, owner.gid)?;
    handle_file_trigger(path, &clean_path, filename, &trigger, &version_dir, owner)
}

fn handle_directory_trigger(
    path: &Path,
    clean_path: &Path,
    filename: &str,
    trigger: &Trigger,
    owner: Owner,
) -> Result<()> {
    if trigger.target_ext != "pdf" {
        return Ok(());
    }

    let original_dir = path.with_extension("");
    if original_dir.exists() {
        return Err(anyhow!(
            "cannot rename source folder back, path exists: {}",
            original_dir.display()
        ));
    }

    handle_folder_to_pdf(path, clean_path)?;
    fs::rename(path, &original_dir).with_context(|| {
        format!(
            "failed to rename source folder {} -> {}",
            path.display(),
            original_dir.display()
        )
    })?;
    notify_owner(
        owner.uid,
        &format!(
            "Created {} and restored source folder name for {}",
            clean_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("output.pdf"),
            filename
        ),
    );
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
    let source_ext = resolve_source_ext(path, &detect_source_ext(path), &trigger.target_ext);
    let out_ext = convertx::output_ext(&trigger.target_ext);

    let builtin_ok = is_valid_target(&mime, &trigger.target_ext);
    let registry_ok = convertx::can_convert(&source_ext, &trigger.target_ext);
    if !builtin_ok && !registry_ok {
        eprintln!(
            "no converter for {} ({source_ext}) -> {}",
            path.display(),
            trigger.target_ext
        );
        return Ok(());
    }

    if let Some(existing) = find_latest_version_by_ext(version_dir, &out_ext) {
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

    let temp_file = path.with_extension(format!("morph_tmp.{out_ext}"));
    let builtin = if builtin_ok {
        morph_engine(path, &temp_file, &out_ext, &source_ext, &mime)
    } else {
        Err(anyhow!("not handled by built-in engine"))
    };
    let status = match builtin {
        Ok(s) => s,
        Err(err) if registry_ok => {
            if builtin_ok {
                eprintln!("built-in engine failed ({err}); trying converter registry");
            }
            let _ = fs::remove_file(&temp_file);
            convertx::convert(path, &temp_file, &source_ext, &trigger.target_ext)?;
            0
        }
        Err(err) => return Err(err),
    };
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
    set_mode(destination, mode)?;
    Ok(())
}

fn handle_folder_to_pdf(input_dir: &Path, output_pdf: &Path) -> Result<()> {
    let owner = Owner::from_path(input_dir)?;
    let workspace = create_pdf_workspace(input_dir, owner.uid, owner.gid)?;
    let temp_dir = workspace.join("pages");
    let final_tmp = workspace.join("merged.pdf");
    let normalized_tmp = workspace.join("normalized.pdf");

    let result = (|| -> Result<()> {
        fs::create_dir_all(&temp_dir)?;
        owner.chown(&temp_dir)?;

        let files = gather_folder_inputs(input_dir);
        if files.is_empty() {
            return Ok(());
        }

        notify_owner(
            owner.uid,
            &format!("Creating PDF from {} files", files.len()),
        );

        for (idx, file) in files.iter().enumerate() {
            let page = temp_dir.join(format!("{:04}.pdf", idx + 1));
            match folder_file_to_pdf(file, &page) {
                Ok(()) => owner.chown(&page)?,
                Err(err) => {
                    // Skip files that cannot become a page rather than
                    // failing the whole folder.
                    eprintln!("skipping {}: {err}", file.display());
                    let _ = fs::remove_file(&page);
                }
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
            return Ok(());
        }

        let mut cmd = tool("pdfunite");
        for page in &pdf_pages {
            cmd.arg(page);
        }
        cmd.arg(&final_tmp);
        run_cmd(&mut cmd)?;

        normalize_and_compress_pdf(&final_tmp, &normalized_tmp)?;
        chown_path(&normalized_tmp, owner.uid, owner.gid)?;
        set_mode(&normalized_tmp, 0o644)?;
        move_file(&normalized_tmp, output_pdf)?;
        Ok(())
    })();

    let _ = fs::remove_dir_all(&workspace);
    result
}

/// Render one folder entry to a single-file PDF page set.
fn folder_file_to_pdf(file: &Path, page: &Path) -> Result<()> {
    let mime = detect_mime(file).unwrap_or_default();
    if mime == "application/pdf" {
        fs::copy(file, page)?;
        return Ok(());
    }
    if mime.starts_with("image/") && run_cmd(tool("magick").arg(file).arg(page)).is_ok() {
        return Ok(());
    }
    let src_ext = resolve_source_ext(file, &detect_source_ext(file), "pdf");
    if is_doc_folder_ext(&src_ext) {
        let r = run_cmd(
            tool("pandoc")
                .arg("-f")
                .arg(pandoc_from_ext(&src_ext))
                .arg(file)
                .arg("-s")
                .arg("--pdf-engine=xelatex")
                .arg("-o")
                .arg(page),
        );
        if r.is_ok() {
            return Ok(());
        }
    }
    let _ = fs::remove_file(page);
    convertx::convert(file, page, &src_ext, "pdf")
}

fn normalize_and_compress_pdf(input_pdf: &Path, output_pdf: &Path) -> Result<()> {
    let mut cmd = tool("gs");
    cmd.arg("-sDEVICE=pdfwrite")
        .arg("-dCompatibilityLevel=1.6")
        .arg(format!("-dPDFSETTINGS={}", PDF_OUTPUT_PROFILE))
        .arg("-dAutoRotatePages=/None")
        .arg("-dDetectDuplicateImages=true")
        .arg("-dCompressFonts=true")
        .arg("-dSubsetFonts=true")
        .arg("-dNOPAUSE")
        .arg("-dQUIET")
        .arg("-dBATCH")
        .arg("-dFIXEDMEDIA")
        .arg("-dPDFFitPage")
        .arg(format!("-sPAPERSIZE={}", PDF_OUTPUT_PAGE_SIZE))
        .arg(format!("-sOutputFile={}", output_pdf.display()))
        .arg(input_pdf);
    run_cmd(&mut cmd)
}

fn create_pdf_workspace(input_dir: &Path, uid: u32, gid: u32) -> Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("clock error")?
        .as_nanos();
    let pid = std::process::id();
    let mut hasher = blake3::Hasher::new();
    hasher.update(input_dir.as_os_str().as_bytes());
    let tag = hasher.finalize().to_hex();
    let path = std::env::temp_dir().join(format!("morph-bang-pdf-{pid}-{ts}-{}", &tag[..12]));
    fs::create_dir_all(&path)?;
    chown_path(&path, uid, gid)?;
    Ok(path)
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
                let input_meta = fs::metadata(input)?;
                chown_path(&dir_path, input_meta.uid(), input_meta.gid())?;
                let mut success = false;
                for i in 0..pages {
                    let page_file = dir_path.join(format!("{:03}.{}", i + 1, target_ext));
                    let in_arg = format!("{}[dpi=300,page={}]", input.display(), i);
                    if run_cmd(tool("vips").arg("copy").arg(in_arg).arg(&page_file)).is_ok()
                    {
                        if copy_owner_and_perms(input, &page_file).is_ok() {
                            success = true;
                        } else {
                            let _ = fs::remove_file(&page_file);
                        }
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
            if run_cmd(tool("vips").arg("copy").arg(in_arg).arg(out)).is_ok() {
                return Ok(0);
            }
        }
        run_cmd(tool("vips").arg("copy").arg(input).arg(out))?;
        return Ok(0);
    }

    if mime.starts_with("video/") || mime.starts_with("audio/") {
        if run_cmd(
            tool("ffmpeg")
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
            tool("ffmpeg")
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
        if !is_pandoc_source(source_ext) && convertx::can_convert(source_ext, target_ext) {
            // Not a pandoc input (pdf, yaml, xlsx, ...): let the registry handle it
            // instead of pandoc guessing markdown.
            return Err(anyhow!("{source_ext} input needs registry converter"));
        }
        let from = pandoc_from_ext(source_ext);
        let mut cmd = tool("pandoc");
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
    chown_path(dst, meta.uid(), meta.gid())?;
    set_mode(dst, meta.permissions().mode())?;
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
    let from_file = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .split('/')
            .next()
            .unwrap_or("")
            .trim_end_matches('?')
            .to_lowercase(),
        _ => String::new(),
    };
    if !from_file.is_empty() {
        return from_file;
    }
    detect_mime(path)
        .ok()
        .map(|m| source_ext_from_mime(&m).to_string())
        .unwrap_or_default()
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
    if mime == "text/x-tex" {
        return "tex";
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
    match mime {
        "image/svg+xml" => "svg",
        "application/xml" | "text/xml" => "xml",
        "application/x-yaml" | "text/yaml" => "yaml",
        "text/csv" => "csv",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/msword" => "doc",
        "application/vnd.oasis.opendocument.spreadsheet" => "ods",
        "application/vnd.oasis.opendocument.presentation" => "odp",
        "application/vnd.ms-outlook" => "msg",
        "text/vcard" => "vcf",
        "model/stl" => "stl",
        "model/obj" => "obj",
        "model/gltf-binary" => "glb",
        "model/gltf+json" => "gltf",
        _ => "",
    }
}

fn is_supported_folder_input(path: &Path) -> bool {
    let mime = match detect_mime(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if mime.starts_with("image/") || mime == "application/pdf" {
        return true;
    }
    let source_ext = resolve_source_ext(path, &detect_source_ext(path), "pdf");
    is_doc_folder_ext(&source_ext) || convertx::can_convert(&source_ext, "pdf")
}

fn pdf_pages(path: &Path) -> Option<u32> {
    let out = tool("pdfinfo").arg(path).output().ok()?;
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
    if uid == 0 {
        return;
    }

    let user = match User::from_uid(Uid::from_raw(uid)) {
        Ok(Some(u)) => u,
        _ => return,
    };
    let bus_path = format!("/run/user/{uid}/bus");
    if !Path::new(&bus_path).exists() {
        return;
    }
    let bus = format!("unix:path={bus_path}");

    let status = Command::new("sudo")
        .arg("-u")
        .arg(&user.name)
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

    match status {
        Ok(s) if s.success() => {}
        Ok(_) => eprintln!("Failed to show notification for uid {}", uid),
        Err(err) => eprintln!("Failed to run notify-send for uid {}: {}", uid, err),
    }
}

pub(crate) fn run_cmd(cmd: &mut Command) -> Result<()> {
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

fn is_pandoc_source(ext: &str) -> bool {
    matches!(ext, "md" | "markdown" | "txt" | "text") || pandoc_from_ext(ext) != "markdown"
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

/// Split `name.!target` / `name.!!target` into `(name, trigger)`.
/// The target may itself contain dots (`video.!av1.mp4`).
fn parse_trigger_name(filename: &str) -> Option<(&str, Trigger)> {
    let idx = filename.rfind(".!")?;
    let stem = &filename[..idx];
    let raw = &filename[idx + 1..];
    if stem.is_empty() {
        return None;
    }
    let lower = raw.to_lowercase();
    let (target, destructive) = if let Some(t) = lower.strip_prefix("!!") {
        (t, true)
    } else {
        (lower.strip_prefix('!')?, false)
    };
    if target.is_empty() || target.starts_with('!') || target.starts_with('.') || target.ends_with('.') {
        return None;
    }
    Some((
        stem,
        Trigger {
            target_ext: target.to_string(),
            destructive,
        },
    ))
}

/// Pick the best source extension: what `file` detects, unless the filename
/// carries a hint (`model.obj.!stl`) that the converter tables understand better.
fn resolve_source_ext(path: &Path, detected: &str, target: &str) -> String {
    let hint = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.rfind(".!").map(|i| &n[..i]).unwrap_or(n))
        .and_then(|stem| Path::new(stem).extension().and_then(|e| e.to_str()))
        .map(|e| e.to_lowercase())
        .filter(|e| convertx::is_known_input(e));
    // An explicit extension in the name is user intent and beats sniffing,
    // which often yields generic answers (text/plain -> md) for data formats.
    match hint {
        Some(h) if convertx::can_convert(&h, target) || detected.is_empty() => h,
        _ => detected.to_string(),
    }
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

fn owner_from_home_path(path: &Path) -> Option<(u32, u32)> {
    let rel = path.strip_prefix("/home").ok()?;
    let user_name = rel.components().next()?.as_os_str().to_str()?;
    if user_name.is_empty() {
        return None;
    }
    let user = User::from_name(user_name).ok()??;
    Some((user.uid.as_raw(), user.gid.as_raw()))
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

/// Rename, falling back to copy+delete across filesystems (e.g. tmpfs /tmp -> /home).
fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).with_context(|| format!("failed to move {} -> {}", src.display(), dst.display()))?;
    let _ = fs::remove_file(src);
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    match fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
        Ok(()) => Ok(()),
        Err(_) if !Uid::effective().is_root() => {
            eprintln!("warning: cannot set mode on {} (not root)", path.display());
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("failed to set mode on {}", path.display())),
    }
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> Result<()> {
    match chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))) {
        Ok(()) => Ok(()),
        // Only root can change ownership; when run unprivileged (e.g. the
        // one-shot `convert` mode) keep going instead of aborting.
        Err(_) if !Uid::effective().is_root() => {
            eprintln!("warning: cannot set ownership on {} (not root)", path.display());
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("failed to set ownership on {}", path.display())),
    }
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
