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

## Features

- On-demand conversion trigger via `.!<ext>`
- Automatic version history in safe mode (`.!<ext>`)
- Destructive override mode (`.!!<ext>`)
- Images, documents, audio, and video conversion
- Fast media remuxing first, then re-encode fallback
- PDF special handling
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
- Documents: `md`, `txt`, `html`, `htm`, `docx`, `odt`, `epub`, `tex`, `rst`, `rtf`, `org`, `textile`, `ipynb`, `typst`

## Engines

- `libvips` + ImageMagick fallback for image workflows
- `ffmpeg` for audio/video workflows
- `pandoc` (+ XeLaTeX) for document workflows
- `poppler` tools for PDF utilities

## Installation

```bash
chmod +x install.sh
./install.sh
```

This installs:
- `/usr/local/bin/morph-bang`
- `morph-bang.service`

## Monitoring

```bash
journalctl -u morph-bang.service -f
```

## Notes

- `.!<ext>` (safe): stores original in version history before conversion.
- Safe mode applies to files and folders (folders are archived before destructive folder->PDF conversion).
- `.!!<ext>` (destructive): converts without storing original.
- If target extension already exists in version history, Morph Bang restores that version instead of reconverting.
- Version store path: `~/.local/share/morph-bang/versions`
- Example: `song.flac` -> `song.!mp3` -> `song.mp3`

## License

MIT
