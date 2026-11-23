# Icon assets

- `icon.jpeg` and `logo.jpeg` are the original artwork.
- `icon.png` is a 512×512 RGB copy for documentation or future UI use.
- `icon.ico` is the multi-resolution Windows executable icon
  (256, 128, 64, 48, 32, 24 and 16 px).
- `logo.png` is a 960 px-wide copy for the README.

Regenerate the derived files from the originals with ImageMagick:

```sh
convert icon.jpeg -background white -alpha remove -resize 512x512 -strip \
  -define png:compression-level=9 icon.png
convert icon.jpeg -background white -alpha remove -resize 256x256 -strip \
  -define icon:auto-resize=256,128,64,48,32,24,16 icon.ico
convert logo.jpeg -resize 960x -strip -define png:compression-level=9 logo.png
```
