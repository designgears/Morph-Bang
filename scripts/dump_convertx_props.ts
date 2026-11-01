// Run with bun from a ConvertX checkout at /tmp/convertx-src (see gen_formats.py).
const names = ["inkscape","libjxl","resvg","vips","libheif","xelatex","calibre","dasel","libreoffice","pandoc","msgconvert","dvisvgm","imagemagick","graphicsmagick","assimp","ffmpeg","potrace","vtracer","vcf","markitdown"];
const out: Record<string, unknown> = {};
for (const n of names) {
  const m = await import(`/tmp/convertx-src/src/converters/${n}.ts`);
  out[n] = m.properties;
}
await Bun.write("/tmp/convertx-props.json", JSON.stringify(out));
