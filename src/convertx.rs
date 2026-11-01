//! Generic converter registry ported from ConvertX.
//!
//! Each converter declares which source extensions it accepts and which
//! targets it can produce (see the generated `formats.rs`). Conversions are
//! attempted in ConvertX's declared priority order until one succeeds.

use crate::formats::{Converter, CONVERTERS};
use crate::{run_cmd, tool};
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Normalize a source extension the way ConvertX does before matching.
pub fn normalize_input(ext: &str) -> String {
    match ext.to_lowercase().as_str() {
        "jfif" | "jpg" => "jpeg".into(),
        "htm" => "html".into(),
        "tex" => "latex".into(),
        "md" => "markdown".into(),
        other => other.to_string(),
    }
}

/// Map a ConvertX target identifier to the file extension written to disk.
/// Some targets (assimp variants, pandoc markdown flavours) are format ids,
/// not real extensions.
pub fn output_ext(target: &str) -> String {
    let t = target.to_lowercase();
    // Compound ffmpeg targets such as `av1.mp4` write the container extension.
    let t = t.rsplit('.').next().unwrap_or(&t).to_string();
    match t.as_str() {
        "jpeg" => "jpg".into(),
        "latex" => "tex".into(),
        "markdown" | "markdown_phpextra" | "markdown_strict" | "markdown_mmd" => "md".into(),
        "glb2" => "glb".into(),
        "gltf2" => "gltf".into(),
        "objnomtl" => "obj".into(),
        "stlb" => "stl".into(),
        "plyb" => "ply".into(),
        "fbxa" => "fbx".into(),
        "assjson" => "json".into(),
        _ => t,
    }
}

/// Target strings to try when matching a user-typed extension against the
/// converter tables: the raw value plus its ConvertX-normalized form.
fn target_candidates(target: &str) -> Vec<String> {
    let t = target.to_lowercase();
    let mut v = vec![t.clone()];
    let n = normalize_input(&t);
    if n != t {
        v.push(n);
    }
    v
}

/// All converters able to turn `from_ext` into `target`, in priority order.
/// Returns `(converter, matched target id)` pairs.
pub fn find_converters(from_ext: &str, target: &str) -> Vec<(&'static Converter, String)> {
    let from = normalize_input(from_ext);
    let candidates = target_candidates(target);
    let mut out = Vec::new();
    for c in CONVERTERS {
        for g in c.groups {
            if !g.from.contains(&from.as_str()) {
                continue;
            }
            if let Some(t) = candidates.iter().find(|t| g.to.contains(&t.as_str())) {
                out.push((c, t.clone()));
                break;
            }
        }
    }
    out
}

pub fn can_convert(from_ext: &str, target: &str) -> bool {
    !find_converters(from_ext, target).is_empty()
}

/// True if any converter accepts this source extension at all.
pub fn is_known_input(ext: &str) -> bool {
    let e = normalize_input(ext);
    CONVERTERS
        .iter()
        .any(|c| c.groups.iter().any(|g| g.from.contains(&e.as_str())))
}

/// Convert `input` to `out` by trying every matching converter in order.
pub fn convert(input: &Path, out: &Path, from_ext: &str, target: &str) -> Result<()> {
    let matches = find_converters(from_ext, target);
    if matches.is_empty() {
        return Err(anyhow!(
            "no converter supports {} -> {}",
            from_ext,
            target
        ));
    }
    let from = normalize_input(from_ext);
    let mut errors = Vec::new();
    for (c, t) in matches {
        match run_converter(c.name, input, out, &from, &t) {
            Ok(()) if out.is_file() => {
                eprintln!("converted {} via {}", input.display(), c.name);
                return Ok(());
            }
            Ok(()) => errors.push(format!("{}: produced no output", c.name)),
            Err(e) => errors.push(format!("{}: {e}", c.name)),
        }
        let _ = fs::remove_file(out);
    }
    Err(anyhow!("all converters failed: {}", errors.join("; ")))
}

fn run_converter(name: &str, input: &Path, out: &Path, from: &str, to: &str) -> Result<()> {
    match name {
        "inkscape" => run_cmd(tool("inkscape").arg(input).arg("-o").arg(out)),
        "libjxl" => {
            let bin = if to == "jxl" { "cjxl" } else { "djxl" };
            run_cmd(tool(bin).arg(input).arg(out))
        }
        "resvg" => run_cmd(tool("resvg").arg(input).arg(out)),
        "vips" => {
            let action = if from == "pdf" { "pdfload" } else { "copy" };
            run_cmd(tool("vips").arg(action).arg(input).arg(out))
        }
        "libheif" => run_cmd(tool("heif-convert").arg(input).arg(out)),
        "xelatex" => {
            let ws = workspace(out)?;
            let r = run_cmd(
                tool("latexmk")
                    .arg("-xelatex")
                    .arg("-interaction=nonstopmode")
                    .arg(format!("-output-directory={}", ws.display()))
                    .arg(input),
            )
            .and_then(|_| collect_output(&ws, input, "pdf", out));
            let _ = fs::remove_dir_all(&ws);
            r
        }
        "calibre" => run_cmd(tool("ebook-convert").arg(input).arg(out)),
        "dasel" => {
            let o = tool("dasel")
                .arg("--var")
                .arg(format!("data={from}:file:{}", input.display()))
                .arg("--out")
                .arg(to)
                .arg("$data")
                .output()
                .context("dasel failed to start")?;
            if !o.status.success() {
                return Err(anyhow!(
                    "dasel failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
            }
            fs::write(out, &o.stdout)?;
            Ok(())
        }
        "libreoffice" => {
            let ws = workspace(out)?;
            let (in_filter, out_filter) = libreoffice_filters(from, to);
            let mut cmd = tool("soffice");
            cmd.arg("--headless")
                .arg(format!("-env:UserInstallation=file://{}/profile", ws.display()));
            if let Some(f) = in_filter {
                cmd.arg(format!("--infilter={f}"));
            }
            let convert_to = match out_filter {
                Some(f) => format!("{to}:{f}"),
                None => to.to_string(),
            };
            cmd.arg("--convert-to")
                .arg(convert_to)
                .arg("--outdir")
                .arg(&ws)
                .arg(input);
            let r = run_cmd(&mut cmd).and_then(|_| collect_output(&ws, input, to, out));
            let _ = fs::remove_dir_all(&ws);
            r
        }
        "pandoc" => {
            let mut cmd = tool("pandoc");
            if to == "pdf" {
                cmd.arg("--pdf-engine=xelatex");
            }
            cmd.arg(input)
                .arg("-f")
                .arg(from)
                .arg("-t")
                .arg(to)
                .arg("-o")
                .arg(out);
            run_cmd(&mut cmd)
        }
        "msgconvert" => run_cmd(tool("msgconvert").arg("--outfile").arg(out).arg(input)),
        "dvisvgm" => {
            let mut cmd = tool("dvisvgm");
            match from {
                "eps" => {
                    cmd.arg("--eps");
                }
                "pdf" => {
                    cmd.arg("--pdf");
                }
                _ => {}
            }
            if to == "svgz" {
                cmd.arg("-z");
            }
            run_cmd(cmd.arg(input).arg("-o").arg(out))
        }
        "imagemagick" => {
            let mut cmd = tool("magick");
            if to == "ico" && from == "svg" {
                cmd.args(["-background", "none", "-density", "512"]);
            }
            if from == "emf" {
                cmd.args(["-define", "emf:delegate=false", "-density", "300"]);
            }
            cmd.arg(input);
            if to == "ico" {
                cmd.args([
                    "-define",
                    "icon:auto-resize=256,128,64,48,32,16",
                    "-background",
                    "none",
                ]);
            }
            if from == "emf" {
                cmd.args(["-background", "white", "-alpha", "remove"]);
            }
            cmd.arg("-auto-orient").arg(out);
            run_cmd(&mut cmd)
        }
        "graphicsmagick" => run_cmd(tool("gm").arg("convert").arg(input).arg(out)),
        "assimp" => run_cmd(
            tool("assimp")
                .arg("export")
                .arg(input)
                .arg(out)
                .arg(format!("-f{to}")),
        ),
        "ffmpeg" => {
            let mut cmd = tool("ffmpeg");
            cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(input);
            if to == "ico" {
                cmd.args([
                    "-filter:v",
                    "scale='min(256,iw)':min'(256,ih)':force_original_aspect_ratio=decrease",
                ]);
            }
            if let Some((codec, _container)) = to.split_once('.') {
                let lib = match codec {
                    "av1" => Some("libaom-av1"),
                    "h264" => Some("libx264"),
                    "h265" => Some("libx265"),
                    "h266" => Some("libx266"),
                    _ => None,
                };
                if let Some(lib) = lib {
                    cmd.args(["-c:v", lib]);
                }
            }
            run_cmd(cmd.arg(out))
        }
        "potrace" => run_cmd(tool("potrace").arg(input).arg("-o").arg(out).arg("-b").arg(to)),
        "vtracer" => run_cmd(tool("vtracer").arg("--input").arg(input).arg("--output").arg(out)),
        "vcf" => vcf_to_csv(input, out),
        "markitdown" => run_cmd(tool("markitdown").arg(input).arg("-o").arg(out)),
        other => Err(anyhow!("unknown converter {other}")),
    }
}

/// Scratch directory next to the temp file for tools that only take `--outdir`.
fn workspace(out: &Path) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let tag = out
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("out")
        .replace(['/', ' '], "_");
    let dir = base.join(format!("morph-bang-ws-{}-{tag}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Move `<input stem>.<ext>` written into `dir` to the requested output path.
fn collect_output(dir: &Path, input: &Path, ext: &str, out: &Path) -> Result<()> {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("invalid input name"))?;
    let expected = dir.join(format!("{stem}.{}", output_ext(ext)));
    let produced = if expected.is_file() {
        expected
    } else {
        // Fall back to any single file with the right extension.
        let want = output_ext(ext);
        fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some(want.as_str()))
            .ok_or_else(|| anyhow!("converter produced no {want} output"))?
    };
    if fs::rename(&produced, out).is_err() {
        fs::copy(&produced, out)?;
        let _ = fs::remove_file(&produced);
    }
    Ok(())
}

fn libreoffice_filters(from: &str, to: &str) -> (Option<&'static str>, Option<&'static str>) {
    if to == "pdf" {
        return (None, None);
    }
    let text = |e: &str| -> Option<Option<&'static str>> {
        Some(match e {
            "602" => Some("T602Document"),
            "abw" => Some("AbiWord"),
            "csv" => Some("Text"),
            "doc" => Some("MS Word 97"),
            "docm" => Some("MS Word 2007 XML VBA"),
            "docx" => Some("MS Word 2007 XML"),
            "dot" => Some("MS Word 97 Vorlage"),
            "dotx" | "dotm" => Some("MS Word 2007 XML Template"),
            "epub" => Some("EPUB"),
            "fb2" => Some("Fictionbook 2"),
            "fodt" => Some("OpenDocument Text Flat XML"),
            "htm" | "html" => Some("HTML (StarWriter)"),
            "hwp" => Some("writer_MIZI_Hwp_97"),
            "mcw" | "mw" => Some("MacWrite"),
            "mwd" => Some("Mariner_Write"),
            "lwp" => Some("LotusWordPro"),
            "lrf" => Some("BroadBand eBook"),
            "odt" => Some("writer8"),
            "ott" => Some("writer8_template"),
            "pages" => Some("Apple Pages"),
            "pdf" => Some("writer_pdf_import"),
            "psw" => Some("PocketWord File"),
            "rtf" => Some("Rich Text Format"),
            "sdw" => Some("StarOffice_Writer"),
            "stw" => Some("writer_StarOffice_XML_Writer_Template"),
            "sxw" => Some("StarOffice XML (Writer)"),
            "tab" | "tsv" | "txt" => Some("Text"),
            "wn" => Some("WriteNow"),
            "wpd" => Some("WordPerfect"),
            "wps" => None,
            "wpt" => Some("MS Word 97 Vorlage"),
            "wri" => Some("MS_Write"),
            "xhtml" => Some("HTML (StarWriter)"),
            "xml" => Some("OpenDocument Text Flat XML"),
            "zabw" => Some("AbiWord"),
            _ => return None,
        })
    };
    let calc = |e: &str| -> Option<Option<&'static str>> {
        Some(match e {
            "csv" | "tsv" => Some("Text - txt - csv (StarCalc)"),
            "ods" => Some("calc8"),
            "ots" => Some("calc8_template"),
            "pdf" => Some("calc_pdf_Export"),
            "xls" => Some("MS Excel 97"),
            "xlsx" => Some("Calc MS Excel 2007 XML"),
            "xlsm" => Some("Calc MS Excel 2007 XML VBA"),
            "xlt" => Some("MS Excel 97 Vorlage"),
            "xltm" => Some("Calc MS Excel 2007 XML Template"),
            _ => return None,
        })
    };
    if let (Some(i), Some(o)) = (text(from), text(to)) {
        return (i, o);
    }
    if let (Some(i), Some(o)) = (calc(from), calc(to)) {
        return (i, o);
    }
    (None, None)
}

/// Port of ConvertX's vCard -> CSV converter.
fn vcf_to_csv(input: &Path, out: &Path) -> Result<()> {
    let data = fs::read_to_string(input)?;
    let mut contacts: Vec<Vec<(&'static str, String)>> = Vec::new();
    for card in data.split("BEGIN:VCARD").skip(1) {
        let card = card.split("END:VCARD").next().unwrap_or("");
        let mut contact: Vec<(&'static str, String)> = Vec::new();
        let mut set = |k: &'static str, v: String| {
            if let Some(e) = contact.iter_mut().find(|(kk, _)| *kk == k) {
                e.1 = v;
            } else {
                contact.push((k, v));
            }
        };
        for line in card.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim().to_string());
            if key == "FN" {
                set("Full Name", value);
            } else if key == "N" {
                let mut parts = value.split(';');
                let last = parts.next().unwrap_or("").to_string();
                let first = parts.next().unwrap_or("").to_string();
                set("Last Name", last);
                set("First Name", first);
            } else if key.starts_with("TEL") {
                set("Phone", value);
            } else if key.starts_with("EMAIL") {
                set("Email", value);
            } else if key == "ORG" {
                set("Organization", value.split(';').next().unwrap_or("").to_string());
            }
        }
        if !contact.is_empty() {
            contacts.push(contact);
        }
    }
    if contacts.is_empty() {
        return Err(anyhow!("no contacts found in vCard"));
    }
    let headers: Vec<&str> = contacts[0].iter().map(|(k, _)| *k).collect();
    let esc = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut csv = headers.iter().map(|h| esc(h)).collect::<Vec<_>>().join(",");
    csv.push('\n');
    for c in &contacts {
        let row: Vec<String> = headers
            .iter()
            .map(|h| {
                esc(c
                    .iter()
                    .find(|(k, _)| k == h)
                    .map(|(_, v)| v.as_str())
                    .unwrap_or(""))
            })
            .collect();
        csv.push_str(&row.join(","));
        csv.push('\n');
    }
    fs::write(out, csv)?;
    Ok(())
}
