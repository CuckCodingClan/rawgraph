# Nodes:

## Source:
- directory selection
- checkbox for "include subfolders"

## Metadata Filter (image routing based on metadata)
- Mode
- ...

## Merge (merge multiple sources)
- collision handling (for when images with the same name are input)

## Layer (one input per layer)
- Mode (Match (Example: input 1 has 10 images and input 2 has 5 images > 1-5 get matched with 1-5 and 6-10 get again matched with 1-5), All (create every possible combination))
- Composite Mode per layer (normal, add, multiply, subtract...)
- Opacity per layer
- toggle layer on/off
- scaling per layer (crop, fit (default), fill, stretch)
- scaling filter per layer (linear, bicubic...)

## Develop
- Exposure
- Contrast
- Lights
- Shadows
- White
- Black
- Whitebalance
- Tint
- Dynamic
- Saturation

## Curves
- Lum x Lum
- Sat x Sat
- Hue x Hue
- Hue x Sat

## Lens Correction
- Mode (auto,)
- Vignette
- Chromatic aberration

## Export
- Format (jpeg, heic, avif, png...)
- Size
- Output Color Space
- Metadata (detailed selection of what to include or change individual ones)
- Output Folder
- Collision handling (skip, overwrite, rename)