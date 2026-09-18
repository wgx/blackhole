use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use glob::glob;
use image::{GrayImage, ImageBuffer};
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const VERSIONED_MAGIC_HEADER: &[u8; 4] = b"GS8C";
const RANGE_MAGIC_HEADER: &[u8; 4] = b"GS8R";
const TILED_MAGIC_HEADER: &[u8; 4] = b"GS8T";
const FORMAT_VERSION: u8 = 1;
const MODE_RAW: u8 = 0;
const MODE_ADAPTIVE: u8 = 6;
const TILE_SHARED_MODEL: u8 = 0;
const TILE_LOCAL_MODEL: u8 = 1;
const TILE_CODEC_RANGE: u8 = 0;
const TILE_CODEC_DEFLATE: u8 = 1;
const ARITHMETIC_HALF: u32 = 0x8000_0000;
const ARITHMETIC_QUARTER: u32 = 0x4000_0000;
const ARITHMETIC_THREE_QUARTER: u32 = 0xC000_0000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("Usage:");
        eprintln!("  Compress:   blackhole -c <input_image> [output_file.bhol]");
        eprintln!("  Decompress: blackhole -d <input_file.bhol> <output_image.png>");
        std::process::exit(1);
    }

    let flag = &args[1];
    let input_pattern = &args[2];
    let output_path = args.get(3).map(PathBuf::from);
    let input_paths = matching_paths(input_pattern)?;

    if input_paths.is_empty() {
        return Err(format!("No files matched input pattern: {}", input_pattern).into());
    }

    if flag == "-d" && output_path.is_none() {
        return Err("An output image is required when decompressing".into());
    }

    if output_path.is_some() && input_paths.len() > 1 {
        return Err("An explicit output file can only be used with one input file".into());
    }
    match flag.as_str() {
        "-c" => {
            for input_path in &input_paths {
                let output = output_path
                    .clone()
                    .unwrap_or_else(|| input_path.with_extension("bhol"));
                compress(input_path, &output)?;
            }
        }
        "-d" => decompress(&input_paths[0], output_path.as_ref().unwrap())?,
        _ => return Err("Invalid option. Use -c to compress or -d to decompress.".into()),
    }

    Ok(())
}

fn matching_paths(pattern: &str) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    Ok(glob(pattern)?.collect::<Result<Vec<_>, _>>()?)
}

fn predict(filter: u8, left: u8, top: u8, _top_left: u8) -> u8 {
    match filter {
        0 => 0,
        1 => left,
        2 => top,
        3 => ((left as u16 + top as u16) / 2) as u8,
        _ => unreachable!("invalid filter"),
    }
}

fn zigzag_encode(residual: u8) -> u8 {
    let signed = residual as i8 as i16;
    ((signed << 1) ^ (signed >> 7)) as u8
}

fn zigzag_decode(value: u8) -> u8 {
    let signed = ((value >> 1) as i16) ^ -((value & 1) as i16);
    signed as i8 as u8
}

struct BitContextModel {
    zeros: Vec<u16>,
    ones: Vec<u16>,
}

impl BitContextModel {
    fn new() -> Self {
        Self {
            zeros: vec![1; 256 * 256],
            ones: vec![1; 256 * 256],
        }
    }

    fn counts(&self, context: u8, prefix: u8) -> (u32, u32) {
        let index = context as usize * 256 + prefix as usize;
        (self.zeros[index] as u32, self.ones[index] as u32)
    }

    fn update(&mut self, context: u8, prefix: u8, bit: bool) {
        let index = context as usize * 256 + prefix as usize;
        let total = self.zeros[index] as u32 + self.ones[index] as u32;
        if total >= u16::MAX as u32 {
            self.zeros[index] = (self.zeros[index] / 2).max(1);
            self.ones[index] = (self.ones[index] / 2).max(1);
        }
        if bit {
            self.ones[index] += 1;
        } else {
            self.zeros[index] += 1;
        }
    }
}

struct BitWriter {
    output: Vec<u8>,
    current: u8,
    bits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            output: Vec::new(),
            current: 0,
            bits: 0,
        }
    }

    fn write(&mut self, bit: bool) {
        self.current = (self.current << 1) | bit as u8;
        self.bits += 1;
        if self.bits == 8 {
            self.output.push(self.current);
            self.current = 0;
            self.bits = 0;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.current <<= 8 - self.bits;
            self.output.push(self.current);
        }
        self.output
    }
}

struct BitReader<'a> {
    input: &'a [u8],
    position: usize,
    current: u8,
    bits: u8,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            position: 0,
            current: 0,
            bits: 0,
        }
    }

    fn read(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        if self.bits == 0 {
            self.current = *self
                .input
                .get(self.position)
                .ok_or("Range-coded payload is truncated")?;
            self.position += 1;
            self.bits = 8;
        }
        self.bits -= 1;
        Ok((self.current & (1 << self.bits)) != 0)
    }
}

struct ArithmeticEncoder {
    low: u32,
    high: u32,
    pending: usize,
    writer: BitWriter,
}

impl ArithmeticEncoder {
    fn new() -> Self {
        Self {
            low: 0,
            high: u32::MAX,
            pending: 0,
            writer: BitWriter::new(),
        }
    }

    fn emit(&mut self, bit: bool) {
        self.writer.write(bit);
        for _ in 0..self.pending {
            self.writer.write(!bit);
        }
        self.pending = 0;
    }

    fn encode(&mut self, bit: bool, zeros: u32, ones: u32) {
        let total = zeros + ones;
        let interval = self.high as u64 - self.low as u64 + 1;
        let split = self.low + (interval * zeros as u64 / total as u64) as u32 - 1;
        if bit {
            self.low = split + 1;
        } else {
            self.high = split;
        }

        loop {
            if self.high < ARITHMETIC_HALF {
                self.emit(false);
            } else if self.low >= ARITHMETIC_HALF {
                self.emit(true);
                self.low -= ARITHMETIC_HALF;
                self.high -= ARITHMETIC_HALF;
            } else if self.low >= ARITHMETIC_QUARTER && self.high < ARITHMETIC_THREE_QUARTER {
                self.pending += 1;
                self.low -= ARITHMETIC_QUARTER;
                self.high -= ARITHMETIC_QUARTER;
            } else {
                break;
            }
            self.low <<= 1;
            self.high = (self.high << 1) | 1;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.pending += 1;
        if self.low < ARITHMETIC_QUARTER {
            self.emit(false);
        } else {
            self.emit(true);
        }
        self.writer.finish()
    }
}

struct ArithmeticDecoder<'a> {
    low: u32,
    high: u32,
    value: u32,
    reader: BitReader<'a>,
}

impl<'a> ArithmeticDecoder<'a> {
    fn new(input: &'a [u8]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut reader = BitReader::new(input);
        let mut value = 0;
        for _ in 0..32 {
            value = (value << 1) | reader.read()? as u32;
        }
        Ok(Self {
            low: 0,
            high: u32::MAX,
            value,
            reader,
        })
    }

    fn decode(&mut self, zeros: u32, ones: u32) -> Result<bool, Box<dyn std::error::Error>> {
        let total = zeros + ones;
        let interval = self.high as u64 - self.low as u64 + 1;
        let split = self.low + (interval * zeros as u64 / total as u64) as u32 - 1;
        let bit = if self.value <= split {
            self.high = split;
            false
        } else {
            self.low = split + 1;
            true
        };

        loop {
            if self.high < ARITHMETIC_HALF {
            } else if self.low >= ARITHMETIC_HALF {
                self.low -= ARITHMETIC_HALF;
                self.high -= ARITHMETIC_HALF;
                self.value -= ARITHMETIC_HALF;
            } else if self.low >= ARITHMETIC_QUARTER && self.high < ARITHMETIC_THREE_QUARTER {
                self.low -= ARITHMETIC_QUARTER;
                self.high -= ARITHMETIC_QUARTER;
                self.value -= ARITHMETIC_QUARTER;
            } else {
                break;
            }
            self.low <<= 1;
            self.high = (self.high << 1) | 1;
            self.value = (self.value << 1) | self.reader.read()? as u32;
        }
        Ok(bit)
    }
}

fn range_encode(payload: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let payload_length = u64::try_from(payload.len())?;
    let mut encoder = ArithmeticEncoder::new();
    let mut model = BitContextModel::new();
    let mut context = 0;

    for &symbol in payload {
        let mut prefix = 0;
        for shift in (0..8).rev() {
            let bit = ((symbol >> shift) & 1) != 0;
            let (zeros, ones) = model.counts(context, prefix);
            encoder.encode(bit, zeros, ones);
            model.update(context, prefix, bit);
            prefix = (prefix << 1) | bit as u8;
        }
        context = symbol;
    }

    let encoded = encoder.finish();
    let mut output = Vec::with_capacity(13 + encoded.len());
    output.extend_from_slice(RANGE_MAGIC_HEADER);
    output.push(FORMAT_VERSION);
    output.extend_from_slice(&payload_length.to_le_bytes());
    output.extend_from_slice(&encoded);
    output.extend_from_slice(&[0; 4]);
    Ok(output)
}

fn range_decode(container: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if container.len() < 14 || &container[0..4] != RANGE_MAGIC_HEADER {
        return Err("Invalid range-coded file header".into());
    }
    if container[4] != FORMAT_VERSION {
        return Err("Unsupported range-coded format version".into());
    }

    let length = u64::from_le_bytes(container[5..13].try_into()?);
    let length = usize::try_from(length).map_err(|_| "Payload is too large")?;
    let mut decoder = ArithmeticDecoder::new(&container[13..])?;
    let mut model = BitContextModel::new();
    let mut context = 0;
    let mut output = Vec::with_capacity(length);

    for _ in 0..length {
        let mut symbol = 0;
        let mut prefix = 0;
        for _ in 0..8 {
            let (zeros, ones) = model.counts(context, prefix);
            let bit = decoder.decode(zeros, ones)?;
            model.update(context, prefix, bit);
            symbol = (symbol << 1) | bit as u8;
            prefix = symbol;
        }
        context = symbol;
        output.push(symbol);
    }

    Ok(output)
}

fn deflate_encode(payload: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(payload)?;
    Ok(encoder.finish()?)
}

fn make_header(mode: u8, width: u32, height: u32) -> Vec<u8> {
    let mut header = Vec::with_capacity(14);
    header.extend_from_slice(VERSIONED_MAGIC_HEADER);
    header.push(FORMAT_VERSION);
    header.push(mode);
    header.extend_from_slice(&width.to_le_bytes());
    header.extend_from_slice(&height.to_le_bytes());
    header
}

fn filtered_data(pixels: &[u8], width: u32, height: u32, filter: u8) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let mut data = Vec::with_capacity(pixels.len());

    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            let left = if x > 0 { pixels[index - 1] } else { 0 };
            let top = if y > 0 { pixels[index - width] } else { 0 };
            let top_left = if x > 0 && y > 0 {
                pixels[index - width - 1]
            } else {
                0
            };
            data.push(zigzag_encode(
                pixels[index].wrapping_sub(predict(filter, left, top, top_left)),
            ));
        }
    }

    data
}

fn adaptive_filtered_data(pixels: &[u8], width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
    let width = width as usize;
    let height = height as usize;
    let mut filters = Vec::with_capacity(height);
    let mut data = Vec::with_capacity(pixels.len());

    for y in 0..height {
        let row = &pixels[y * width..(y + 1) * width];
        let mut best_filter = 0;
        let mut best_score = u64::MAX;

        for filter in 0..=3 {
            let mut score = 0u64;
            for x in 0..width {
                let index = y * width + x;
                let left = if x > 0 { row[x - 1] } else { 0 };
                let top = if y > 0 { pixels[index - width] } else { 0 };
                let top_left = if x > 0 && y > 0 {
                    pixels[index - width - 1]
                } else {
                    0
                };
                score +=
                    zigzag_encode(row[x].wrapping_sub(predict(filter, left, top, top_left))) as u64;
            }
            if score < best_score {
                best_score = score;
                best_filter = filter;
            }
        }

        filters.push(best_filter);
        for x in 0..width {
            let index = y * width + x;
            let left = if x > 0 { row[x - 1] } else { 0 };
            let top = if y > 0 { pixels[index - width] } else { 0 };
            let top_left = if x > 0 && y > 0 {
                pixels[index - width - 1]
            } else {
                0
            };
            data.push(zigzag_encode(row[x].wrapping_sub(predict(
                best_filter,
                left,
                top,
                top_left,
            ))));
        }
    }

    (filters, data)
}

fn tile_regions(width: u32, height: u32, tile_size: u32) -> Vec<(u32, u32, u32, u32)> {
    let mut regions = Vec::new();
    let mut tile_y = 0;
    while tile_y < height {
        let mut tile_x = 0;
        let tile_height = (height - tile_y).min(tile_size);
        while tile_x < width {
            regions.push((tile_x, tile_y, (width - tile_x).min(tile_size), tile_height));
            tile_x += tile_size;
        }
        tile_y += tile_size;
    }
    regions
}

fn make_tiled_header(
    model: u8,
    codec: u8,
    tile_size: u32,
    width: u32,
    height: u32,
    tile_count: usize,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let tile_size = u16::try_from(tile_size).map_err(|_| "Tile size is too large")?;
    let tile_count = u32::try_from(tile_count).map_err(|_| "Too many tiles")?;
    let mut header = Vec::with_capacity(21);
    header.extend_from_slice(TILED_MAGIC_HEADER);
    header.push(FORMAT_VERSION);
    header.push(model);
    header.push(codec);
    header.extend_from_slice(&tile_size.to_le_bytes());
    header.extend_from_slice(&width.to_le_bytes());
    header.extend_from_slice(&height.to_le_bytes());
    header.extend_from_slice(&tile_count.to_le_bytes());
    Ok(header)
}

fn make_tile_payload(pixels: &[u8], width: u32, region: (u32, u32, u32, u32)) -> Vec<u8> {
    let (tile_x, tile_y, tile_width, tile_height) = region;
    let width = width as usize;
    let mut tile_pixels = Vec::with_capacity((tile_width * tile_height) as usize);
    for row in 0..tile_height as usize {
        let start = (tile_y as usize + row) * width + tile_x as usize;
        tile_pixels.extend_from_slice(&pixels[start..start + tile_width as usize]);
    }

    let (filters, data) = adaptive_filtered_data(&tile_pixels, tile_width, tile_height);
    let mut payload = filters;
    payload.extend_from_slice(&data);
    payload
}

fn encode_tiled_candidate(
    pixels: &[u8],
    width: u32,
    height: u32,
    tile_size: u32,
    model: u8,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let regions = tile_regions(width, height, tile_size);
    let tile_payloads: Vec<_> = regions
        .iter()
        .map(|&region| make_tile_payload(pixels, width, region))
        .collect();

    if model == TILE_SHARED_MODEL {
        let mut payload = Vec::new();
        for tile in &tile_payloads {
            payload.extend_from_slice(tile);
        }
        let range = range_encode(&payload)?;
        let deflate = deflate_encode(&payload)?;
        let (codec, encoded) = if range.len() <= deflate.len() {
            (TILE_CODEC_RANGE, range)
        } else {
            (TILE_CODEC_DEFLATE, deflate)
        };
        let mut output = make_tiled_header(
            TILE_SHARED_MODEL,
            codec,
            tile_size,
            width,
            height,
            regions.len(),
        )?;
        output.extend_from_slice(&encoded);
        return Ok(output);
    }

    let mut output = make_tiled_header(
        TILE_LOCAL_MODEL,
        u8::MAX,
        tile_size,
        width,
        height,
        regions.len(),
    )?;
    for tile in tile_payloads {
        let range = range_encode(&tile)?;
        let deflate = deflate_encode(&tile)?;
        let (codec, encoded) = if range.len() <= deflate.len() {
            (TILE_CODEC_RANGE, range)
        } else {
            (TILE_CODEC_DEFLATE, deflate)
        };
        output.push(codec);
        output.extend_from_slice(&u32::try_from(encoded.len())?.to_le_bytes());
        output.extend_from_slice(&encoded);
    }
    Ok(output)
}

fn encode_new_payload(
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut candidates = Vec::new();

    let mut raw = make_header(MODE_RAW, width, height);
    raw.extend_from_slice(pixels);
    candidates.push(range_encode(&raw)?);
    candidates.push(deflate_encode(&raw)?);

    for filter in 0..=3 {
        let mut candidate = make_header(filter + 1, width, height);
        candidate.extend_from_slice(&filtered_data(pixels, width, height, filter));
        candidates.push(range_encode(&candidate)?);
        candidates.push(deflate_encode(&candidate)?);
    }

    let (filters, data) = adaptive_filtered_data(pixels, width, height);
    let mut adaptive = make_header(MODE_ADAPTIVE, width, height);
    adaptive.extend_from_slice(&filters);
    adaptive.extend_from_slice(&data);
    candidates.push(range_encode(&adaptive)?);
    candidates.push(deflate_encode(&adaptive)?);

    for tile_size in [64, 128, 256, 512] {
        candidates.push(encode_tiled_candidate(
            pixels,
            width,
            height,
            tile_size,
            TILE_SHARED_MODEL,
        )?);
        candidates.push(encode_tiled_candidate(
            pixels,
            width,
            height,
            tile_size,
            TILE_LOCAL_MODEL,
        )?);
    }

    candidates
        .into_iter()
        .min_by_key(Vec::len)
        .ok_or_else(|| "No compression candidates were generated".into())
}

fn compress(input_path: &Path, output_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "Compressing {} -> {}",
        input_path.display(),
        output_path.display()
    );
    println!("  Reading image...");
    let img = image::open(input_path)?.to_luma8();
    let (width, height) = img.dimensions();
    let pixels = img.as_raw();

    println!("  Evaluating compression candidates...");
    let compressed_payload = encode_new_payload(pixels, width, height)?;
    println!("  Writing compressed file...");
    let mut out_file = File::create(output_path)?;
    out_file.write_all(&compressed_payload)?;

    println!(
        "Compressed {} to {} successfully.",
        input_path.display(),
        output_path.display()
    );
    Ok(())
}

fn pixel_count(width: u32, height: u32) -> Result<usize, Box<dyn std::error::Error>> {
    (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| "Image dimensions are too large".into())
}

fn decode_new_payload(payload: &[u8]) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error>> {
    if payload.len() < 14 || &payload[0..4] != VERSIONED_MAGIC_HEADER {
        return Err("Invalid versioned file header".into());
    }
    if payload[4] != FORMAT_VERSION {
        return Err("Unsupported .bhol format version".into());
    }

    let mode = payload[5];
    let width = u32::from_le_bytes(payload[6..10].try_into()?);
    let height = u32::from_le_bytes(payload[10..14].try_into()?);
    let count = pixel_count(width, height)?;
    let width_usize = width as usize;
    let mut offset = 14;

    if mode == MODE_RAW {
        let data = payload.get(offset..).ok_or("Missing raw pixel data")?;
        if data.len() != count {
            return Err("Raw pixel data has an invalid length".into());
        }
        return Ok((width, height, data.to_vec()));
    }

    let filters = if mode == MODE_ADAPTIVE {
        let end = offset
            .checked_add(height as usize)
            .ok_or("Filter metadata is too large")?;
        let filters = payload.get(offset..end).ok_or("Missing filter metadata")?;
        offset = end;
        filters.to_vec()
    } else if (1..=4).contains(&mode) {
        vec![mode - 1; height as usize]
    } else {
        return Err("Unknown .bhol compression mode".into());
    };

    let data = payload.get(offset..).ok_or("Missing filtered pixel data")?;
    if data.len() != count {
        return Err("Filtered pixel data has an invalid length".into());
    }

    let mut reconstructed = Vec::with_capacity(count);
    for index in 0..count {
        let x = index % width_usize;
        let y = index / width_usize;
        let left = if x > 0 { reconstructed[index - 1] } else { 0 };
        let top = if y > 0 {
            reconstructed[index - width_usize]
        } else {
            0
        };
        let top_left = if x > 0 && y > 0 {
            reconstructed[index - width_usize - 1]
        } else {
            0
        };
        let residual = zigzag_decode(data[index]);
        reconstructed.push(residual.wrapping_add(predict(filters[y], left, top, top_left)));
    }

    Ok((width, height, reconstructed))
}

fn decode_coded_payload(codec: u8, encoded: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    match codec {
        TILE_CODEC_RANGE => range_decode(encoded),
        TILE_CODEC_DEFLATE => {
            let mut decoder = DeflateDecoder::new(encoded);
            let mut payload = Vec::new();
            decoder.read_to_end(&mut payload)?;
            Ok(payload)
        }
        _ => Err("Unknown tile codec".into()),
    }
}

fn decode_tile_into(
    tile_payload: &[u8],
    region: (u32, u32, u32, u32),
    width: u32,
    output: &mut [u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let (tile_x, tile_y, tile_width, tile_height) = region;
    let tile_pixel_count = pixel_count(tile_width, tile_height)?;
    let filter_data = tile_payload
        .get(..tile_height as usize)
        .ok_or("Missing tile filter data")?;
    if filter_data.iter().any(|&filter| filter > 3) {
        return Err("Unknown tile filter".into());
    }
    let residuals = tile_payload
        .get(tile_height as usize..)
        .ok_or("Missing tile residual data")?;
    if residuals.len() != tile_pixel_count {
        return Err("Tile residual data has an invalid length".into());
    }

    let tile_width = tile_width as usize;
    let tile_height = tile_height as usize;
    let image_width = width as usize;
    let mut tile_pixels = Vec::with_capacity(tile_pixel_count);
    for index in 0..tile_pixel_count {
        let x = index % tile_width;
        let y = index / tile_width;
        let left = if x > 0 { tile_pixels[index - 1] } else { 0 };
        let top = if y > 0 {
            tile_pixels[index - tile_width]
        } else {
            0
        };
        let top_left = if x > 0 && y > 0 {
            tile_pixels[index - tile_width - 1]
        } else {
            0
        };
        tile_pixels.push(zigzag_decode(residuals[index]).wrapping_add(predict(
            filter_data[y],
            left,
            top,
            top_left,
        )));
    }

    for row in 0..tile_height {
        let output_start = (tile_y as usize + row) * image_width + tile_x as usize;
        let tile_start = row * tile_width;
        output[output_start..output_start + tile_width]
            .copy_from_slice(&tile_pixels[tile_start..tile_start + tile_width]);
    }
    Ok(())
}

fn decode_tiled_container(
    container: &[u8],
) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error>> {
    if container.len() < 21 || !container.starts_with(TILED_MAGIC_HEADER) {
        return Err("Invalid tiled file header".into());
    }
    if container[4] != FORMAT_VERSION {
        return Err("Unsupported tiled format version".into());
    }

    let model = container[5];
    let codec = container[6];
    let tile_size = u16::from_le_bytes(container[7..9].try_into()?) as u32;
    let width = u32::from_le_bytes(container[9..13].try_into()?);
    let height = u32::from_le_bytes(container[13..17].try_into()?);
    let tile_count = u32::from_le_bytes(container[17..21].try_into()?) as usize;
    let regions = tile_regions(width, height, tile_size);
    if regions.len() != tile_count {
        return Err("Tiled archive has an invalid tile count".into());
    }

    let mut tile_payloads = Vec::with_capacity(tile_count);
    let mut offset = 21;
    if model == TILE_SHARED_MODEL {
        if codec != TILE_CODEC_RANGE && codec != TILE_CODEC_DEFLATE {
            return Err("Unknown shared tile codec".into());
        }
        tile_payloads.push(decode_coded_payload(codec, &container[offset..])?);
    } else if model == TILE_LOCAL_MODEL {
        for _ in 0..tile_count {
            let tile_codec = *container.get(offset).ok_or("Missing tile codec")?;
            offset += 1;
            let length_end = offset.checked_add(4).ok_or("Tile length overflow")?;
            let length = u32::from_le_bytes(
                container
                    .get(offset..length_end)
                    .ok_or("Missing tile length")?
                    .try_into()?,
            ) as usize;
            offset = length_end;
            let tile_end = offset.checked_add(length).ok_or("Tile payload overflow")?;
            let encoded = container
                .get(offset..tile_end)
                .ok_or("Missing tile payload")?;
            tile_payloads.push(decode_coded_payload(tile_codec, encoded)?);
            offset = tile_end;
        }
    } else {
        return Err("Unknown tiled model".into());
    }

    let mut output = vec![0; pixel_count(width, height)?];
    if model == TILE_SHARED_MODEL {
        let mut payload_offset: usize = 0;
        for region in regions {
            let tile_height = region.3 as usize;
            let tile_pixels = pixel_count(region.2, region.3)?;
            let tile_length = tile_height + tile_pixels;
            let tile_end = payload_offset
                .checked_add(tile_length)
                .ok_or("Tile data overflow")?;
            let tile_payload = tile_payloads[0]
                .get(payload_offset..tile_end)
                .ok_or("Missing shared tile data")?;
            decode_tile_into(tile_payload, region, width, &mut output)?;
            payload_offset = tile_end;
        }
        if payload_offset != tile_payloads[0].len() {
            return Err("Shared tile data has an invalid length".into());
        }
    } else {
        for (region, tile_payload) in regions.into_iter().zip(tile_payloads) {
            decode_tile_into(&tile_payload, region, width, &mut output)?;
        }
    }

    Ok((width, height, output))
}

fn decode_payload(payload: &[u8]) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error>> {
    if payload.starts_with(VERSIONED_MAGIC_HEADER) {
        decode_new_payload(payload)
    } else {
        Err("Invalid file header: Not a recognized .bhol archive".into())
    }
}

fn decode_container(container: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if container.starts_with(RANGE_MAGIC_HEADER) {
        return range_decode(container);
    }

    let mut decoder = DeflateDecoder::new(container);
    let mut payload = Vec::new();
    decoder.read_to_end(&mut payload)?;
    Ok(payload)
}

fn decode_image_container(
    container: &[u8],
) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error>> {
    if container.starts_with(TILED_MAGIC_HEADER) {
        decode_tiled_container(container)
    } else {
        decode_payload(&decode_container(container)?)
    }
}

fn decompress(input_path: &Path, output_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "Decompressing {} -> {}",
        input_path.display(),
        output_path.display()
    );
    println!("  Reading compressed file...");
    let mut in_file = File::open(input_path)?;
    let mut buffer = Vec::new();
    in_file.read_to_end(&mut buffer)?;

    println!("  Reconstructing image...");
    let (width, height, reconstructed) = decode_image_container(&buffer)?;

    println!("  Writing image...");
    let img_buffer: GrayImage = ImageBuffer::from_raw(width, height, reconstructed)
        .ok_or("Failed to construct image buffer from payload")?;
    img_buffer.save(output_path)?;

    println!(
        "Decompressed {} to {} successfully.",
        input_path.display(),
        output_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_round_trips_every_residual() {
        for residual in 0..=u8::MAX {
            assert_eq!(zigzag_decode(zigzag_encode(residual)), residual);
        }
    }

    #[test]
    fn new_payload_round_trips_structured_pixels() {
        let width = 17;
        let height = 9;
        let pixels: Vec<u8> = (0..width * height)
            .map(|index| ((index / width) * 7 + index % width * 3) as u8)
            .collect();
        let compressed = encode_new_payload(&pixels, width as u32, height as u32).unwrap();

        let (decoded_width, decoded_height, decoded) = decode_image_container(&compressed).unwrap();
        assert_eq!(
            (decoded_width, decoded_height),
            (width as u32, height as u32)
        );
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn new_payload_round_trips_noisy_pixels() {
        let pixels: Vec<u8> = (0..128).map(|index| (index * 73 + 19) as u8).collect();
        let compressed = encode_new_payload(&pixels, 16, 8).unwrap();

        let (_, _, decoded) = decode_image_container(&compressed).unwrap();
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn new_payload_round_trips_large_constant_image() {
        let pixels = vec![42; 128 * 1024];
        let compressed = encode_new_payload(&pixels, 128, 1024).unwrap();

        let (_, _, decoded) = decode_image_container(&compressed).unwrap();
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn sample_pngs_round_trip() {
        let mut paths: Vec<_> = std::fs::read_dir("samples")
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "png"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "no PNG samples found");

        for path in paths {
            let image = image::open(&path).unwrap().to_luma8();
            let (width, height) = image.dimensions();
            let pixels = image.as_raw();
            let selected = encode_new_payload(pixels, width, height).unwrap();
            let (_, _, decoded) = decode_image_container(&selected).unwrap();

            assert_eq!(decoded, *pixels);
            let description = if selected.starts_with(TILED_MAGIC_HEADER) {
                let model = match selected[5] {
                    TILE_SHARED_MODEL => "shared",
                    TILE_LOCAL_MODEL => "local",
                    _ => "unknown",
                };
                let tile_size = u16::from_le_bytes(selected[7..9].try_into().unwrap());
                format!("tiled {tile_size}x{tile_size} {model}")
            } else {
                "whole-image".to_string()
            };
            eprintln!(
                "{}: selected={}, {}, dimensions={}x{}",
                path.display(),
                selected.len(),
                description,
                width,
                height
            );
        }
    }
}
