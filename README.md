# Morph 🧪

**Morph** is a deep-integrated Linux daemon that makes file extensions honest. If you rename `photo.png` to `photo.webp`, Morph intercepts the event at the kernel level and re-writes the actual file data to match the new extension instantly.

Built specifically for high-performance Arch-based systems like **CachyOS**, it leverages optimized engines to ensure that renaming a file feels like a native OS feature.

## ✨ Features

- **Universal Conversion:** Handles Images, Documents, Video, and Audio.
- **Intelligent Media Swapping:** Uses FFmpeg "Remuxing" (Stream Copying) to change video containers (`.mkv` to `.mp4`) in milliseconds with zero quality loss.
- **PDF ↔ Images:** Multi-page PDF extraction and folder-to-PDF compilation.
- **Deep Document Processing:** Powered by Pandoc (with XeLaTeX support) to morph `.md` to `.pdf`, `.docx`, or `.html`.
- **System-Wide:** Works in the Terminal, Dolphin (KDE), Thunar, or any application.
- **Safety First:**
  - Prevents infinite loops via hash-based locking.
  - Detects and ignores browser temporary/part files.
  - Preserves original file ownership and permissions (no "root-owned" files).

## 🔄 Special Conversions

### PDF → Images
Rename a PDF to an image extension to extract all pages:

```
document.pdf  →  document.png
```

**Creates:**
```
document/
├── 001.png
├── 002.png
├── 003.png
└── ... (one per page at 300 DPI)
```

### Folder → PDF
Rename a folder of files to `.pdf` to combine them:

```
my_folder/  →  my_folder.pdf
```

**Supports:** Images (PNG, JPG, WebP, TIFF, GIF, AVIF, HEIC, JXL) + Documents (MD, TXT, HTML, DOCX, ODT, EPUB, TEX, RST, RTF, ORG, Textile, IPYNB, Typst)

Files are combined in natural sort order by filename.

## 🚀 Engines

### 1. LibVips (Images)
High-performance image processor with ImageMagick backend fallback.

| Read & Write | Read-Only (rasterize) | Via ImageMagick |
|--------------|----------------------|-----------------|
| PNG, JPEG, WebP, AVIF, HEIC/HEIF, TIFF, GIF, JPEG-XL, JPEG2000, HDR, PPM/PGM/PBM/PFM, FITS | PDF, SVG, OpenEXR | BMP, ICO, PSD, TGA, EPS, DDS, RAW camera (CR2, NEF, ARW, DNG, etc.) |

### 2. FFmpeg (Media)
Intelligent remuxing (zero quality loss) with fallback to re-encoding.

| Read & Write | Read-Only | Encoding Strategy |
|--------------|-----------|-------------------|
| MP4, MKV, MOV, AVI, WebM, FLV, TS, MPG, OGV, 3GP, GIF | APE, AA/AAX (Audible), WMA, RA | Remux first, encode if incompatible |
| MP3, FLAC, WAV, OGG, M4A, AAC, Opus, AC3, DTS, AIFF, WV | | Audio: quality-based VBR encoding |
| | | Video: x264/VP9 with CRF quality |

### 3. Pandoc (Documents)
Full bidirectional document conversion with XeLaTeX PDF support.

| Bidirectional (R/W) | Output Only | Input Only |
|---------------------|-------------|------------|
| MD, HTML, DOCX, ODT, EPUB, LaTeX/TeX, RST, RTF, Org, MediaWiki, Textile, FB2, Jupyter, JIRA, OPML, JSON, Typst, Djot, Man | PDF, PPTX, AsciiDoc, Beamer, ICML, TEI, Texinfo | RIS, EndNote XML, TSV, CSV, txt2tags, Creole, TWiki, TikiWiki, VimWiki |

## 🛠️ Installation

1. **Clone the repo:**
   ```bash
   git clone https://github.com/designgears/morph.git
   cd morph
   ```

2. **Run the installer:**
   ```bash
   chmod +x install.sh
   ./install.sh
   ```

### Dependencies
Automatically installed: `inotify-tools`, `libvips`, `imagemagick`, `pandoc`, `ffmpeg`, `poppler`, `libnotify`, `texlive-bin`, `texlive-xetex`

## 📈 Monitoring
Watch Morph work in real-time:
```bash
journalctl -u morph.service -f
```

## ⚖️ License
MIT
