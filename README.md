# Blackhole ⚫ [Experimental]

Blackhole is a lossless image compressor for 8-bit (256 colours) grayscale images.

**Highly** Optimized for black and white photography, anything else will give poor results.

Input images are converted to 8-bit luminance before compression. Decompression reconstructs the grayscale pixels exactly.

**Note: this is only lossless for 8-bit images, not 16-bit.**

## Features

- Lossless grayscale compression
- Adaptive filtering using raw, horizontal, vertical, and average predictors
- Zigzag encoding for signed residuals
- Context-aware binary arithmetic coding
- DEFLATE fallback when it produces a smaller file
- Adaptive tiled compression for photographic images
- Wildcard input patterns for batch compression

## Requirements

- Rust and Cargo

## Build

```text
cargo build --release
```

The executable is written to `target/release/blackhole` on Unix-like systems or `target/release/blackhole.exe` on Windows.

## Usage

Compress one image and specify the output file:

```text
blackhole -c input.png output.bhol
```

Compress one image without specifying an output file:

```text
blackhole -c input.png
```

This creates `input.bhol` beside the source image.

Compress multiple matching images. Quote the pattern so Blackhole expands it consistently across shells:

```text
blackhole -c "samples/*.png"
```

Each input receives a `.bhol` file in its own directory.

Decompress an archive:

```text
blackhole -d input.bhol output.png
```

The decompression command requires an explicit output image path.

## Testing

Run the unit tests:

```text
cargo test
```

The test suite checks residual coding, arithmetic-coded round trips, large constant images, and every PNG in `samples`.

## Format Compatibility

The current version writes and reads the versioned `GS8C`, arithmetic-coded `GS8R`, and adaptive tiled `GS8T` formats. The compressor compares whole-image and tiled candidates and keeps the smallest result. Older legacy archives are not supported.

## No Warranty

This software is provided without warranty of any kind, express or implied, including any warranty that it will work as intended.
