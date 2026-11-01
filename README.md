# Morph Bang 🧪💥

**Morph Bang** is a Linux daemon that makes file extensions honest on demand.
Rename a file or folder to `.!<ext>` (example: `photo.!webp`), and Morph Bang converts the underlying data to that format, then removes the `!` automatically.
Use `.!!<ext>` to force destructive conversion (skip version backup).

## How It Works

1. Rename a file or folder to a bang extension:
   - `image.png` -> `image.!jpg`
   - `image.png` -> `image.!!jpg` (destructive mode)
   - `video.mkv` -> `video.!mp4`
   - `notes.md` -> `notes.!pdf`
   - `album/` -> `album.!pdf`
2. Morph Bang detects the rename.
3. It converts to the requested target format.
4. It writes the final output without the bang:
   - `image.jpg`
   - `video.mp4`
   - `notes.pdf`
   - `album.pdf`

Only names with `.!<ext>` or `.!!<ext>` are tracked.

Targets may contain dots for codec-qualified media (`video.!h264.mp4`, `video.!av1.mkv`), and
the source extension can be kept in the name as a hint when content sniffing is ambiguous
(`model.obj.!stl`, `config.yaml.!json`). The hint stays in the output name (`model.obj.stl`).

## Features

- On-demand conversion trigger via `.!<ext>`
- Automatic version history in safe mode (`.!<ext>`)
- Destructive override mode (`.!!<ext>`)
- Images, documents, audio, video, 3D models, data files, ebooks, spreadsheets, contacts and email
- Every conversion [ConvertX](https://github.com/C4illin/ConvertX) supports, via the same tools
- Fast media remuxing first, then re-encode fallback
- PDF special handling
- Folder->PDF output normalization (uniform page size + moderate compression)
- Preserves ownership and permissions where possible

## Special Cases

### Multi-page PDF -> Image Set

Rename a multi-page PDF to an image target:

`document.pdf` -> `document.!png`

Morph Bang will extract pages into a folder named from the file base:

```text
document/
├── 001.png
├── 002.png
├── 003.png
└── ...
```

### Folder -> Single PDF

Rename a folder to `.!pdf`:

`my_folder/` -> `my_folder.!pdf`

Morph Bang converts supported files in natural filename order and merges them into:

`my_folder.pdf`

Supported folder inputs include common images and docs such as:
- Images: `png`, `jpg`, `jpeg`, `webp`, `tiff`, `tif`, `bmp`, `gif`, `avif`, `heic`, `jxl`
- PDFs: `pdf`
- Documents: `md`, `txt`, `html`, `htm`, `docx`, `odt`, `epub`, `tex`, `rst`, `rtf`, `org`, `textile`, `ipynb`, `typst`

## Engines

Morph Bang has two tiers:

1. **Built-in paths** tuned for the common cases:
   - `libvips` + ImageMagick fallback for image workflows
   - `ffmpeg` (remux first, re-encode fallback) for audio/video workflows
   - `pandoc` (+ XeLaTeX) for document workflows
   - `poppler` tools for PDF utilities
   - `ghostscript` for final PDF normalization/compression
2. **Converter registry** ported from ConvertX (`src/formats.rs`, generated). When the
   built-in path does not apply or fails, every registry converter that accepts the source
   and can produce the target is tried in ConvertX's priority order:

   | Converter | Examples |
   |---|---|
   | inkscape, resvg, dvisvgm | svg/pdf/eps/emf/wmf -> png/pdf/svg/dxf/... |
   | libjxl, libheif | jxl and heic/avif decode/encode |
   | vips, imagemagick, graphicsmagick | hundreds of raster formats, `ico` with multi-size icons |
   | ffmpeg | 470+ input containers, codec targets like `h264.mp4`, `av1.mkv` |
   | pandoc, xelatex (latexmk), calibre, libreoffice, markitdown | docx/odt/doc/rtf/epub/mobi/xls(x)/pptx/pdf/md/... |
   | dasel | json/yaml/toml/csv/xml data conversion |
   | assimp | 3D models: obj/stl/glb/gltf/fbx/dae/ply/3ds/... |
   | potrace, vtracer | raster -> vector tracing |
   | msgconvert, vcf | Outlook `.msg` -> `.eml`, vCard -> csv |

   The full from/to tables live in `src/formats.rs`. Regenerate them from a ConvertX checkout with
   `scripts/dump_convertx_props.ts` and `scripts/gen_formats.py`.

   Known gaps inherited from the toolbox: no HEIC *encoding* (the bundled vips has no HEVC encoder),
   and raster -> svg goes through ImageMagick (embedded bitmap) before the tracers.

These tools can run either natively on the host or inside a Docker toolbox container.

### Native engine (default)

All tools are installed from the distro package manager. Fastest startup, smallest footprint.
The installer adds the registry tools available in the Arch repos; converters whose binary is
missing are skipped, so the set of supported formats depends on what is installed.

### Docker engine

Morph Bang runs the same tools inside a persistent container built from the
[ConvertX](https://github.com/C4illin/ConvertX) image, which bundles libvips, ImageMagick,
ffmpeg, pandoc, XeLaTeX, poppler and ghostscript. Only `inotifywait`, `file`, `notify-send`
and Docker itself are needed on the host, so this works on any distro.

- The container is named `morph-bang-engine`, created once at daemon startup and reused
  for every conversion (`docker exec`, about 20 ms overhead per tool call).
- `/home` and `/tmp` are bind-mounted at the same paths, so no path translation is needed.
- The container idles on `sleep infinity`; the ConvertX web UI is never started.
- Configured via environment variables on the service:
  - `MORPH_ENGINE=docker|native` (default `native`)
  - `MORPH_DOCKER_IMAGE` (default `ghcr.io/c4illin/convertx:latest`)
- Changing `MORPH_DOCKER_IMAGE` and restarting the service recreates the container.

## Installation

```bash
chmod +x install.sh
./install.sh            # native engine
./install.sh --docker   # docker toolbox engine
```

This installs:
- `/usr/local/bin/morph-bang`
- `morph-bang.service`
- (docker mode) the `morph-bang-engine` container, created on first service start

## Monitoring

```bash
journalctl -u morph-bang.service -f
```

## One-shot mode

Process trigger paths once without the watcher, useful for testing an engine:

```bash
MORPH_ENGINE=docker morph-bang convert ~/photo.!webp ~/model.obj.!stl
```

## Notes

- `.!<ext>` (safe): stores original in version history before conversion.
- Safe mode versioning applies to files.
- `.!!<ext>` (destructive): converts without storing original.
- If target extension already exists in version history, Morph Bang restores that version instead of reconverting.
- Folder -> PDF is non-destructive: Morph Bang writes `name.pdf` and renames `name.!pdf` back to `name`.
- Folder -> PDF temporary working files are created under `/tmp` and cleaned up automatically.
- Version store path: `~/.local/share/morph-bang/versions`
- Example: `song.flac` -> `song.!mp3` -> `song.mp3`
