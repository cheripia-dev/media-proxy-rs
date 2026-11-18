use std::io::Cursor;
use std::time::Duration;

use image::{AnimationDecoder, DynamicImage, GenericImage, GenericImageView, ImageEncoder};

use crate::{Encoded, ProcessError, ProcessErrorKind, ProcessOptions, ProcessorConfig};

/// Header-only dimension probe (no pixel allocation). Returns None for
/// formats the `image` crate cannot guess (JXL/JP2/JXR have per-path checks).
pub(crate) fn probe_dimensions(src: &[u8]) -> Option<(u32, u32)> {
	let reader = image::ImageReader::new(Cursor::new(src))
		.with_guessed_format()
		.ok()?;
	reader.into_dimensions().ok()
}

/// Shared decode-dimension policy (also used for SVG-embedded rasters, M-01).
pub(crate) fn dimensions_allowed_for(max_decode_pixels: u64, width: u64, height: u64) -> bool {
	if width == 0 || height == 0 {
		return false;
	}
	const MAX_SIDE: u64 = 32768;
	if width > MAX_SIDE || height > MAX_SIDE {
		return false;
	}
	match width.checked_mul(height) {
		Some(pixels) => pixels <= max_decode_pixels,
		None => false,
	}
}

fn le24(b: &[u8]) -> u32 {
	(b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16)
}

/// Shared animation frame cap. Enforced in three independent places
/// (`webp_animation_within_budget`'s pre-scan, `encode_img`'s in-loop
/// defense-in-depth, and `encode_anim`'s frame-collection loop) that must
/// agree on the same policy value.
pub(crate) const ANIMATION_FRAMES_LIMIT: u64 = 1000;

/// `AnimationDecoder::loop_count()` を WebP の `ANIM` チャンクの loop_count に変換する。
/// WebP は 0 が無限ループ。APNG の `num_plays` / GIF の repeat はそのまま渡す
/// (gif2webp や MNG/JXL 経路と同じ扱い)。仕様上の「ループ回数」の解釈差で
/// 再生回数が ±1 ずれる可能性はあるが、0 を無限として扱う点は共通。
fn webp_loop_count(loop_count: image::metadata::LoopCount) -> u32 {
	match loop_count {
		image::metadata::LoopCount::Infinite => 0,
		image::metadata::LoopCount::Finite(n) => n.get(),
	}
}

fn map_budget_err(e: String) -> ProcessError {
	let kind = if e.contains("FramesLimit") || e.contains("Decode") {
		ProcessErrorKind::DecodeLimit
	} else {
		ProcessErrorKind::Decode
	};
	ProcessError::new(kind, e)
}

/// RIFF/WebP animation pre-scan (M-02).
fn webp_animation_within_budget(data: &[u8], max_decode_pixels: u64) -> Result<(), String> {
	if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
		return Ok(());
	}
	let mut off = 12usize;
	let mut canvas_pixels: Option<u64> = None;
	let mut frames = 0u64;
	while off + 8 <= data.len() {
		let fourcc = &data[off..off + 4];
		let size = u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]])
			as usize;
		let body = off + 8;
		if body.checked_add(size).map_or(true, |end| end > data.len()) {
			break;
		}
		if fourcc == b"VP8X" && size >= 10 {
			let w = 1u64 + le24(&data[body + 4..body + 7]) as u64;
			let h = 1u64 + le24(&data[body + 7..body + 10]) as u64;
			canvas_pixels = Some(w.saturating_mul(h));
		}
		if fourcc == b"ANMF" && size >= 16 {
			frames += 1;
			if frames > ANIMATION_FRAMES_LIMIT {
				return Err(format!("FramesLimit {}>{}", frames, ANIMATION_FRAMES_LIMIT));
			}
			let per_frame = canvas_pixels.unwrap_or(u64::MAX);
			let total = per_frame.saturating_mul(frames);
			if total > max_decode_pixels {
				return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
			}
		}
		off = body + size + (size & 1);
	}
	Ok(())
}

/// APNG事前スキャン。
fn png_apng_within_budget(data: &[u8], max_decode_pixels: u64) -> Result<(), String> {
	const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
	if !data.starts_with(&SIG) {
		return Ok(());
	}
	let mut off = 8usize;
	let mut canvas_pixels: Option<u64> = None;
	while off + 8 <= data.len() {
		let len =
			u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
		let ctype = &data[off + 4..off + 8];
		let body = off + 8;
		let end = match body.checked_add(len).and_then(|e| e.checked_add(4)) {
			Some(end) if end <= data.len() => end,
			_ => break,
		};
		if ctype == b"IHDR" && len >= 8 {
			let w = u32::from_be_bytes([data[body], data[body + 1], data[body + 2], data[body + 3]])
				as u64;
			let h = u32::from_be_bytes([
				data[body + 4],
				data[body + 5],
				data[body + 6],
				data[body + 7],
			]) as u64;
			canvas_pixels = Some(w.saturating_mul(h));
		}
		if ctype == b"acTL" && len >= 4 {
			let frames =
				u32::from_be_bytes([data[body], data[body + 1], data[body + 2], data[body + 3]])
					as u64;
			if frames > ANIMATION_FRAMES_LIMIT {
				return Err(format!("FramesLimit {}>{}", frames, ANIMATION_FRAMES_LIMIT));
			}
			let total = canvas_pixels.unwrap_or(u64::MAX).saturating_mul(frames);
			if total > max_decode_pixels {
				return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
			}
			return Ok(());
		}
		if ctype == b"IDAT" {
			break;
		}
		off = end;
	}
	Ok(())
}

/// GIF事前スキャン。
fn gif_animation_within_budget(data: &[u8], max_decode_pixels: u64) -> Result<(), String> {
	if data.len() < 13 || !(data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
		return Ok(());
	}
	let packed = data[10];
	let mut off = 13usize;
	if packed & 0x80 != 0 {
		let gct_bytes = (2usize << (packed & 0x07)) * 3;
		off = match off.checked_add(gct_bytes) {
			Some(off) if off <= data.len() => off,
			_ => return Ok(()),
		};
	}
	let mut frames = 0u64;
	let mut decoded_pixels = 0u64;
	loop {
		let Some(&tag) = data.get(off) else {
			return Ok(());
		};
		match tag {
			0x21 => {
				off += 2;
				loop {
					let Some(&block_size) = data.get(off) else {
						return Ok(());
					};
					off += 1;
					if block_size == 0 {
						break;
					}
					off = match off.checked_add(block_size as usize) {
						Some(off) if off <= data.len() => off,
						_ => return Ok(()),
					};
				}
			}
			0x2C => {
				frames += 1;
				if frames > ANIMATION_FRAMES_LIMIT {
					return Err(format!("FramesLimit {}>{}", frames, ANIMATION_FRAMES_LIMIT));
				}
				if off + 10 > data.len() {
					return Ok(());
				}
				let rect_w = u16::from_le_bytes([data[off + 5], data[off + 6]]) as u64;
				let rect_h = u16::from_le_bytes([data[off + 7], data[off + 8]]) as u64;
				let (screen_w, screen_h) = (
					u16::from_le_bytes([data[6], data[7]]) as u64,
					u16::from_le_bytes([data[8], data[9]]) as u64,
				);
				if rect_w > screen_w || rect_h > screen_h {
					return Err(format!(
						"FrameRect {}x{} exceeds screen {}x{}",
						rect_w, rect_h, screen_w, screen_h
					));
				}
				decoded_pixels = decoded_pixels.saturating_add(rect_w.saturating_mul(rect_h));
				if decoded_pixels > max_decode_pixels {
					return Err(format!(
						"DecodePixels {}>{}",
						decoded_pixels, max_decode_pixels
					));
				}
				let local_packed = data[off + 9];
				off += 10;
				if local_packed & 0x80 != 0 {
					let lct_bytes = (2usize << (local_packed & 0x07)) * 3;
					off = match off.checked_add(lct_bytes) {
						Some(off) if off <= data.len() => off,
						_ => return Ok(()),
					};
				}
				off += 1;
				loop {
					let Some(&block_size) = data.get(off) else {
						return Ok(());
					};
					off += 1;
					if block_size == 0 {
						break;
					}
					off = match off.checked_add(block_size as usize) {
						Some(off) if off <= data.len() => off,
						_ => return Ok(()),
					};
				}
			}
			_ => return Ok(()),
		}
	}
}

pub(crate) fn image_size_hint(opts: &ProcessOptions, cfg: &ProcessorConfig) -> (u32, u32) {
	const WEBP_MAX_DIMENSION: u32 = 16383;
	let (mut width, mut height) = if opts.emoji {
		(WEBP_MAX_DIMENSION, 128)
	} else if opts.avatar {
		(WEBP_MAX_DIMENSION, 320)
	} else if opts.r#static {
		(498, 422)
	} else if opts.preview {
		(200, 200)
	} else if opts.badge {
		(96, 96)
	} else {
		(cfg.max_pixels, cfg.max_pixels)
	};
	if let Some(w) = opts.width {
		width = w;
	}
	if let Some(h) = opts.height {
		height = h;
	}
	(width, height)
}

/// WebP 可逆圧縮がネイティブに扱える形式かを判定する。
/// WebP 可逆圧縮は 8bit RGB/RGBA のみをサポートしている。
fn is_webp_lossless_compatible(img: &DynamicImage) -> bool {
	matches!(
		img,
		DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_)
	)
}

/// ビット深度や色空間を維持したままリサイズする。
/// WebP 可逆圧縮に対応していない入力を PNG で出力する際に使う。
fn resize_lossless(
	img: DynamicImage,
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Option<DynamicImage> {
	let (width, height) = image_size_hint(opts, cfg);
	let max_width = width.min(img.width());
	let max_height = height.min(img.height());
	if img.dimensions() == (max_width, max_height) {
		return Some(img);
	}
	let filter = cfg.filter_type.into();
	Some(img.resize(max_width, max_height, filter))
}

pub(crate) fn resize(
	img: DynamicImage,
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Option<DynamicImage> {
	let (width, height) = image_size_hint(opts, cfg);
	if opts.badge {
		let img = if img.dimensions() == (width, height) {
			img
		} else {
			resize_inner(img, width, height, cfg.filter_type.into())?
		};
		let img = img.into_luma8();
		let mut canvas = image::GrayAlphaImage::new(width, height);
		let x_start = (width - img.width()) / 2;
		let y_start = (height - img.height()) / 2;
		let mut sub_canvas = canvas.sub_image(x_start, y_start, width - x_start, height - y_start);
		let mut y = 0;
		for rows in img.rows() {
			let mut x = 0;
			for p in rows {
				let p: image::LumaA<u8> = [p.0[0], p.0[0]].into();
				sub_canvas.put_pixel(x, y, p);
				x += 1;
			}
			y += 1;
		}
		return Some(DynamicImage::ImageLumaA8(canvas));
	}
	let max_width = width.min(img.width());
	let max_height = height.min(img.height());
	let filter = cfg.filter_type.into();
	if img.dimensions() == (max_width, max_height) {
		return Some(img);
	}
	resize_inner(img, max_width, max_height, filter)
}

pub(crate) fn encode_raster(
	src: &[u8],
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
	codec: image::ImageFormat,
) -> Result<Encoded, ProcessError> {
	if let Ok((w, h)) = image::ImageReader::with_format(Cursor::new(src), codec).into_dimensions() {
		if !dimensions_allowed_for(cfg.max_decode_pixels, w as u64, h as u64) {
			return Err(ProcessError::new(
				ProcessErrorKind::DecodeLimit,
				format!("DecodeDimensions {}x{} over limit", w, h),
			));
		}
	}
	let icc = crate::color::extract_raster_icc(src, codec);
	if opts.r#static || opts.badge {
		return encode_single(src, opts, cfg, codec, icc);
	}
	match codec {
		image::ImageFormat::Png => {
			let a = match image::codecs::png::PngDecoder::new(Cursor::new(src)) {
				Ok(a) => a,
				Err(_) => return encode_single(src, opts, cfg, codec, icc),
			};
			if !a.is_apng().unwrap_or(false) {
				return encode_single(src, opts, cfg, codec, icc);
			}
			png_apng_within_budget(src, cfg.max_decode_pixels).map_err(map_budget_err)?;
			match a.apng() {
				Ok(frames) => {
					let loop_count = webp_loop_count(frames.loop_count());
					encode_anim(frames.into_frames(), loop_count, opts, cfg, icc)
				}
				Err(_) => encode_single(src, opts, cfg, codec, icc),
			}
		}
		image::ImageFormat::Gif => {
			gif_animation_within_budget(src, cfg.max_decode_pixels).map_err(map_budget_err)?;
			match image::codecs::gif::GifDecoder::new(Cursor::new(src)) {
				Ok(a) => {
					let loop_count = webp_loop_count(a.loop_count());
					encode_anim(a.into_frames(), loop_count, opts, cfg, icc)
				}
				Err(_) => encode_single(src, opts, cfg, codec, icc),
			}
		}
		image::ImageFormat::WebP => {
			let a = match image::codecs::webp::WebPDecoder::new(Cursor::new(src)) {
				Ok(a) => a,
				Err(_) => return encode_single(src, opts, cfg, codec, icc),
			};
			if a.has_animation() {
				webp_animation_within_budget(src, cfg.max_decode_pixels).map_err(map_budget_err)?;
				let decoder = webp::AnimDecoder::new(src);
				if let Ok(mut dec) = decoder.decode() {
					let mut offset = 0;
					let mut frames = vec![];
					dec.sort_by_time_stamp();
					for frame in dec.into_iter() {
						if frames.len() >= ANIMATION_FRAMES_LIMIT as usize {
							return Err(ProcessError::new(
								ProcessErrorKind::DecodeLimit,
								format!("FramesLimit {}", ANIMATION_FRAMES_LIMIT),
							));
						}
						let img = if frame.get_layout().is_alpha() {
							let Some(image) = image::ImageBuffer::from_raw(
								frame.width(),
								frame.height(),
								frame.get_image().to_owned(),
							) else {
								continue;
							};
							image
						} else {
							let Some(image) = image::ImageBuffer::from_raw(
								frame.width(),
								frame.height(),
								frame.get_image().to_owned(),
							) else {
								continue;
							};
							DynamicImage::ImageRgb8(image).into_rgba8()
						};
						let delay = frame.get_time_ms() - offset;
						offset = frame.get_time_ms();
						if delay < 0 {
							continue;
						}
						let delay = Duration::from_millis(delay as u64);
						let delay = image::Delay::from_saturating_duration(delay);
						let frame = image::Frame::from_parts(img, 0, 0, delay);
						frames.push(Ok(frame));
					}
					let frames = image::Frames::new(Box::new(frames.into_iter()));
					encode_anim(frames, dec.loop_count, opts, cfg, icc)
				} else {
					let loop_count = webp_loop_count(a.loop_count());
					encode_anim(a.into_frames(), loop_count, opts, cfg, icc)
				}
			} else {
				encode_single(src, opts, cfg, codec, icc)
			}
		}
		#[cfg(feature = "avif-decoder")]
		image::ImageFormat::Avif => encode_avif_seq(src, opts, cfg),
		_ => encode_single(src, opts, cfg, codec, icc),
	}
}

pub(crate) fn encode_jxl(
	src: &[u8],
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Result<Encoded, ProcessError> {
	let mut image = jxl_oxide::JxlImage::builder()
		.read(Cursor::new(src))
		.map_err(|e| ProcessError::new(ProcessErrorKind::Decode, format!("JpegXL {:?}", e)))?;
	let has_black = image.pixel_format().has_black();
	if has_black || !opts.lossless {
		image.request_color_encoding(jxl_oxide::EnumColourEncoding::srgb(
			jxl_oxide::RenderingIntent::Relative,
		));
	}
	// 可逆出力では色変換せず、レンダリング結果を説明する ICC プロファイルを
	// そのまま転送する。CMYK (has_black) は WebP で表現できないため変換する。
	let icc = if opts.lossless && !has_black {
		Some(image.rendered_icc())
	} else {
		None
	};
	let (w, h) = (image.width(), image.height());
	if !dimensions_allowed_for(cfg.max_decode_pixels, w as u64, h as u64) {
		return Err(ProcessError::new(
			ProcessErrorKind::DecodeLimit,
			format!("DecodeDimensions {}x{} over limit", w, h),
		));
	}
	let keyframes = image.num_loaded_keyframes();
	let animated = image.image_header().metadata.animation.is_some() && keyframes > 1;
	if animated && !image.is_loading_done() {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"JpegXLTruncated",
		));
	}
	if !animated {
		let render = image
			.render_frame(0)
			.map_err(|e| ProcessError::new(ProcessErrorKind::Decode, format!("JpegXL {:?}", e)))?;
		let img = jxl_render_to_image(&render).ok_or_else(|| {
			ProcessError::new(ProcessErrorKind::Decode, "JpegXLUnsupportedPixelFormat")
		})?;
		return response_img(img, src, None, opts, cfg, icc);
	}
	let canvas_pixels = (w as u64).saturating_mul(h as u64);
	let frame_count = keyframes as u64;
	if frame_count > ANIMATION_FRAMES_LIMIT {
		return Err(ProcessError::new(
			ProcessErrorKind::DecodeLimit,
			format!("FramesLimit {}>{}", frame_count, ANIMATION_FRAMES_LIMIT),
		));
	}
	let total = canvas_pixels.saturating_mul(frame_count);
	if total > cfg.max_decode_pixels {
		return Err(ProcessError::new(
			ProcessErrorKind::DecodeLimit,
			format!("DecodePixels {}>{}", total, cfg.max_decode_pixels),
		));
	}
	let anim = image.image_header().metadata.animation.as_ref().unwrap();
	let tps_num = (anim.tps_numerator as u64).max(1);
	let tps_den = anim.tps_denominator as u64;
	let loop_count = anim.num_loops;
	let mut collected: Vec<Result<image::Frame, image::ImageError>> =
		Vec::with_capacity(keyframes.min(ANIMATION_FRAMES_LIMIT as usize));
	for keyframe in 0..keyframes {
		if collected.len() >= ANIMATION_FRAMES_LIMIT as usize {
			return Err(ProcessError::new(
				ProcessErrorKind::DecodeLimit,
				format!("FramesLimit {}", ANIMATION_FRAMES_LIMIT),
			));
		}
		let render = image
			.render_frame(keyframe)
			.map_err(|e| ProcessError::new(ProcessErrorKind::Decode, format!("JpegXL {:?}", e)))?;
		let img = jxl_render_to_image(&render).ok_or_else(|| {
			ProcessError::new(ProcessErrorKind::Decode, "JpegXLUnsupportedPixelFormat")
		})?;
		let dur_ms = (render.duration() as u64)
			.saturating_mul(tps_den)
			.saturating_mul(1000)
			/ tps_num;
		let delay = image::Delay::from_saturating_duration(Duration::from_millis(dur_ms));
		collected.push(Ok(image::Frame::from_parts(img.into_rgba8(), 0, 0, delay)));
	}
	if collected.is_empty() {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"NoAvailableFrames",
		));
	}
	let frames = image::Frames::new(Box::new(collected.into_iter()));
	encode_anim(frames, loop_count, opts, cfg, icc)
}

#[cfg(feature = "avif-decoder")]
pub(crate) fn encode_avif_seq(
	src: &[u8],
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Result<Encoded, ProcessError> {
	let first_frame_only = opts.r#static || opts.badge;
	let seq = match crate::avif_seq::decode(
		src,
		cfg.max_decode_pixels,
		ANIMATION_FRAMES_LIMIT,
		first_frame_only,
	) {
		Ok(Some(seq)) => seq,
		Ok(None) => {
			let img = image::load_from_memory_with_format(src, image::ImageFormat::Avif).map_err(
				|e| ProcessError::new(ProcessErrorKind::Decode, format!("AvifError {:?}", e)),
			)?;
			let icc = crate::color::extract_raster_icc(src, image::ImageFormat::Avif);
			return response_img(img, src, Some(image::ImageFormat::Avif), opts, cfg, icc);
		}
		Err(e) => {
			return Err(ProcessError::new(
				ProcessErrorKind::Decode,
				format!("AvifSeq {}", e),
			))
		}
	};
	if first_frame_only || seq.frames.len() == 1 {
		let Some(frame) = seq.frames.into_iter().next() else {
			return Err(ProcessError::new(
				ProcessErrorKind::Decode,
				"NoAvailableFrames",
			));
		};
		return response_img(
			DynamicImage::ImageRgba8(frame.image),
			src,
			None,
			opts,
			cfg,
			None,
		);
	}
	let collected: Vec<Result<image::Frame, image::ImageError>> = seq
		.frames
		.into_iter()
		.map(|frame| {
			let delay =
				image::Delay::from_saturating_duration(Duration::from_millis(frame.duration_ms));
			Ok(image::Frame::from_parts(frame.image, 0, 0, delay))
		})
		.collect();
	let frames = image::Frames::new(Box::new(collected.into_iter()));
	encode_anim(frames, 0, opts, cfg, None)
}

pub(crate) fn encode_mng(
	src: &[u8],
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Result<Encoded, ProcessError> {
	let first_frame_only = opts.r#static || opts.badge;
	let anim = crate::mng::decode(
		src,
		cfg.max_decode_pixels,
		ANIMATION_FRAMES_LIMIT,
		first_frame_only,
	)
	.map_err(|e| ProcessError::new(ProcessErrorKind::Decode, format!("MngAnim {}", e)))?;
	if first_frame_only || anim.frames.len() == 1 {
		if let Some(frame) = anim.frames.into_iter().next() {
			return response_img(
				DynamicImage::ImageRgba8(frame.into_buffer()),
				src,
				None,
				opts,
				cfg,
				None,
			);
		}
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"NoAvailableFrames",
		));
	}
	let loop_count = anim.loop_count;
	let frames = image::Frames::new(Box::new(anim.frames.into_iter().map(Ok)));
	encode_anim(frames, loop_count, opts, cfg, None)
}

fn encode_single_avif(rgba: image::RgbaImage) -> Result<Encoded, ProcessError> {
	let mut buf = vec![];
	DynamicImage::ImageRgba8(rgba)
		.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Avif)
		.map_err(|e| ProcessError::new(ProcessErrorKind::Encode, format!("avif encode {:?}", e)))?;
	Ok(Encoded {
		bytes: buf,
		content_type: "image/avif",
		ext: ".avif",
		warning: None,
	})
}

fn webp_config(
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Result<webp::WebPConfig, ProcessError> {
	let mut config = webp::WebPConfig::new()
		.map_err(|_| ProcessError::new(ProcessErrorKind::Encode, "webp config"))?;
	config.quality = opts.webp_quality.unwrap_or(cfg.webp_quality);
	if opts.lossless {
		config.lossless = 1;
		config.alpha_compression = 0;
	}
	Ok(config)
}

pub(crate) fn encode_anim(
	frames: image::Frames,
	loop_count: u32,
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
	icc: Option<Vec<u8>>,
) -> Result<Encoded, ProcessError> {
	// アニメーションは RGBA フレームへ展開されるため、Gray ICC を
	// そのまま RGB WebP に転送できない。sRGB 化した画素を可逆圧縮する。
	let mut icc = icc;
	let gray_icc = if opts.lossless
		&& icc
			.as_deref()
			.and_then(|profile| lcms2::Profile::new_icc(profile).ok())
			.is_some_and(|profile| profile.color_space() == lcms2::ColorSpaceSignature::GrayData)
	{
		icc.take()
	} else {
		None
	};
	// AVIF は image crate 0.25 でアニメーションエンコード未対応のため、
	// アニメーション入力は WebP アニメーションで出力する。
	let conf = webp_config(opts, cfg)?;
	let mut size: Option<(u32, u32)> = None;
	let mut encoder = None;
	let mut available_frames = 0;
	let mut add_frame_err = None;
	let mut timestamp = 0;
	let mut frame_index: u64 = 0;
	for frame in frames {
		if frame_index >= ANIMATION_FRAMES_LIMIT {
			return Err(ProcessError::new(
				ProcessErrorKind::DecodeLimit,
				format!("FramesLimit {}", ANIMATION_FRAMES_LIMIT),
			));
		}
		frame_index += 1;
		if let Ok(frame) = frame {
			// libwebpのタイムスタンプはフレームの表示開始時刻
			let start = timestamp;
			timestamp += Duration::from(frame.delay()).as_millis() as i32;
			let img = DynamicImage::ImageRgba8(frame.into_buffer());
			let img = match gray_icc.as_deref() {
				Some(profile) => crate::color::convert_to_srgb(img, profile).map_err(|_| {
					ProcessError::new(
						ProcessErrorKind::Decode,
						"Gray ICC to sRGB conversion failed",
					)
				})?,
				None => img,
			};
			let img = if !opts.lossless {
				match icc.as_deref() {
					Some(profile) => {
						crate::color::convert_to_srgb(img, profile).unwrap_or_else(|img| img)
					}
					None => img,
				}
			} else {
				img
			};
			let img = resize(img, opts, cfg)
				.ok_or_else(|| ProcessError::new(ProcessErrorKind::Encode, "resize failed"))?;
			if let Some((sw, sh)) = size {
				if sw != img.width() || sh != img.height() {
					continue;
				}
			} else {
				size = Some((img.width(), img.height()));
				let mut enc = webp::AnimEncoder::new(img.width(), img.height(), &conf);
				// WebP の loop_count は u16。範囲外は libwebp が拒否して
				// AnimEncoder::encode() が panic するため、ここで飽和させる。
				enc.set_loop_count(loop_count.min(u16::MAX as u32) as i32);
				encoder = Some(enc);
			}
			if let Ok(aframe) = image_to_frame(&img, start) {
				if let Some(enc) = encoder.as_mut() {
					match enc.add_frame(aframe) {
						Ok(_) => available_frames += 1,
						Err(e) => add_frame_err = Some(format!("{:?}", e)),
					}
				}
			}
		} else {
			break;
		}
	}
	if size.is_none() || encoder.is_none() {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"NoAvailableFrames",
		));
	}
	if available_frames == 0 {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"NoAvailableFrames",
		));
	}
	let mut bytes = encoder.unwrap().encode().to_vec();
	set_last_anmf_duration(&mut bytes, timestamp.max(0) as u32);
	if opts.lossless {
		if let Some(profile) = icc.as_deref() {
			bytes = crate::color::embed_webp_icc(&bytes, profile).ok_or_else(|| {
				ProcessError::new(
					ProcessErrorKind::Unsupported,
					"Cannot preserve ICC profile in RGB WebP output",
				)
			})?;
		}
	}
	Ok(Encoded {
		bytes,
		content_type: "image/webp",
		ext: ".webp",
		warning: add_frame_err,
	})
}

pub(crate) fn encode_single(
	src: &[u8],
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
	codec: image::ImageFormat,
	icc: Option<Vec<u8>>,
) -> Result<Encoded, ProcessError> {
	// ICC プロファイルが CMYK の JPEG は、image crate（zune-jpeg）が CMYK 生データを
	// 返さないため専用経路でデコード・変換する。
	if codec == image::ImageFormat::Jpeg {
		if let Some(profile) = icc.as_deref() {
			let is_cmyk = lcms2::Profile::new_icc(profile)
				.map(|p| p.color_space() == lcms2::ColorSpaceSignature::CmykData)
				.unwrap_or(false);
			if is_cmyk {
				let img = decode_cmyk_jpeg(src, profile, cfg)?;
				return response_img(img, src, Some(codec), opts, cfg, None);
			}
		}
	}

	let max_alloc = cfg.max_decode_pixels.saturating_mul(4);
	let mut reader = image::ImageReader::with_format(Cursor::new(src), codec);
	let mut limits = image::Limits::default();
	limits.max_alloc = Some(max_alloc);
	reader.limits(limits);
	let img = reader
		.decode()
		.map_err(|e| ProcessError::new(ProcessErrorKind::Decode, format!("{:?}", e)))?;
	response_img(img, src, Some(codec), opts, cfg, icc)
}

pub(crate) fn response_img(
	img: DynamicImage,
	src: &[u8],
	codec: Option<image::ImageFormat>,
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
	icc: Option<Vec<u8>>,
) -> Result<Encoded, ProcessError> {
	let img = match codec {
		Some(image::ImageFormat::Jpeg) | Some(image::ImageFormat::Tiff) => exif_rotate(src, img),
		_ => img,
	};
	// 非可逆出力では ICC プロファイルの色空間から sRGB へ変換する。
	// 可逆出力ではピクセルを変更せず、プロファイルを出力側へ転送する。
	let img = if !opts.lossless {
		match icc.as_deref() {
			Some(profile) => crate::color::convert_to_srgb(img, profile).unwrap_or_else(|img| img),
			None => img,
		}
	} else {
		img
	};
	// lossless 指定時、WebP 可逆圧縮でネイティブに表現できない入力
	// (16bit 画像など) は PNG で出力し、ビット深度を維持する。
	if opts.lossless && !opts.badge && !is_webp_lossless_compatible(&img) {
		let img = resize_lossless(img, opts, cfg)
			.ok_or_else(|| ProcessError::new(ProcessErrorKind::Encode, "resize failed"))?;
		let mut buf = vec![];
		let mut encoder = image::codecs::png::PngEncoder::new(&mut buf);
		if let Some(profile) = icc.as_deref() {
			encoder.set_icc_profile(profile.to_vec()).map_err(|e| {
				ProcessError::new(ProcessErrorKind::Encode, format!("png icc {:?}", e))
			})?;
		}
		img.write_with_encoder(encoder).map_err(|e| {
			ProcessError::new(ProcessErrorKind::Encode, format!("png encode {:?}", e))
		})?;
		return Ok(Encoded {
			bytes: buf,
			content_type: "image/png",
			ext: ".png",
			warning: None,
		});
	}
	let img = resize(img, opts, cfg)
		.ok_or_else(|| ProcessError::new(ProcessErrorKind::Encode, "resize failed"))?;
	if opts.badge {
		if badge_entropy(&img.to_luma8()) < 0.1 {
			return Err(ProcessError::new(
				ProcessErrorKind::BadgeSkipped,
				"badge entropy too low",
			));
		}
		let mut buf = vec![];
		DynamicImage::ImageLumaA8(img.into_luma_alpha8())
			.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
			.map_err(|e| {
				ProcessError::new(ProcessErrorKind::Encode, format!("png encode {:?}", e))
			})?;
		return Ok(Encoded {
			bytes: buf,
			content_type: "image/png",
			ext: ".png",
			warning: None,
		});
	}
	let (width, height) = (img.width(), img.height());
	let rgba = img.into_rgba8();
	if opts.accept_avif && cfg.encode_avif && !opts.lossless {
		encode_single_avif(rgba)
	} else {
		let encoder = webp::Encoder::from_rgba(rgba.as_raw(), width, height);
		let config = webp_config(opts, cfg)?;
		let mem = encoder.encode_advanced(&config).map_err(|e| {
			ProcessError::new(ProcessErrorKind::Encode, format!("webp encode {:?}", e))
		})?;
		let mut bytes = mem.to_vec();
		if opts.lossless {
			if let Some(profile) = icc.as_deref() {
				bytes = crate::color::embed_webp_icc(&bytes, profile).ok_or_else(|| {
					ProcessError::new(
						ProcessErrorKind::Unsupported,
						"Cannot preserve ICC profile in RGB WebP output",
					)
				})?;
			}
		}
		Ok(Encoded {
			bytes,
			content_type: "image/webp",
			ext: ".webp",
			warning: None,
		})
	}
}

/// JPEG の APP14 マーカーから Adobe transform 値を取得する。
/// APP14 がなければ None、SOS/EOI に達するか壊れている場合も None を返す。
fn cmyk_jpeg_adobe_transform(src: &[u8]) -> Option<u8> {
	if !src.starts_with(&[0xff, 0xd8]) {
		return None;
	}
	let mut off = 2usize;
	loop {
		if *src.get(off)? != 0xff {
			return None;
		}
		while *src.get(off)? == 0xff {
			off += 1;
		}
		let marker = *src.get(off)?;
		off += 1;
		match marker {
			0xda | 0xd9 => return None,
			0x01 | 0xd0..=0xd8 => continue,
			0x00 => return None,
			_ => {}
		}
		let len = u16::from_be_bytes([*src.get(off)?, *src.get(off + 1)?]) as usize;
		if len < 2 {
			return None;
		}
		let end = off.checked_add(len)?;
		let data = src.get(off + 2..end)?;
		if marker == 0xee && data.len() >= 12 && data.starts_with(b"Adobe") {
			return Some(data[11]);
		}
		off = end;
	}
}

/// CMYK ICC プロファイル付き JPEG をデコードし、sRGB の RgbImage に変換する。
fn decode_cmyk_jpeg(
	src: &[u8],
	icc: &[u8],
	cfg: &ProcessorConfig,
) -> Result<DynamicImage, ProcessError> {
	let input = lcms2::Profile::new_icc(icc).map_err(|e| {
		ProcessError::new(
			ProcessErrorKind::Decode,
			format!("JPEG ICC profile: {:?}", e),
		)
	})?;
	if input.color_space() != lcms2::ColorSpaceSignature::CmykData {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"JPEG ICC profile is not CMYK",
		));
	}

	let adobe = cmyk_jpeg_adobe_transform(src);
	// YCCK (=2) は CMYK の符号化形式であり、CMYK ICC と併用できる。
	// jpeg-decoder が CMYK に戻すため、そのまま後続の色変換に渡す。
	if matches!(adobe, Some(1)) {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"Unsupported Adobe color transform for CMYK ICC JPEG",
		));
	}

	let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(src));
	decoder.read_info().map_err(|e| {
		ProcessError::new(
			ProcessErrorKind::Decode,
			format!("CMYK JPEG header: {:?}", e),
		)
	})?;
	let info = decoder
		.info()
		.ok_or_else(|| ProcessError::new(ProcessErrorKind::Decode, "Missing CMYK JPEG metadata"))?;
	if info.pixel_format != jpeg_decoder::PixelFormat::CMYK32 {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"CMYK ICC attached to a non-CMYK JPEG",
		));
	}

	let (width, height) = (u32::from(info.width), u32::from(info.height));
	if !dimensions_allowed_for(cfg.max_decode_pixels, width as u64, height as u64) {
		return Err(ProcessError::new(
			ProcessErrorKind::DecodeLimit,
			format!("DecodeDimensions {}x{} over limit", width, height),
		));
	}

	let pixels = (width as usize)
		.checked_mul(height as usize)
		.ok_or_else(|| {
			ProcessError::new(ProcessErrorKind::DecodeLimit, "CMYK JPEG size overflow")
		})?;
	let expected = pixels.checked_mul(4).ok_or_else(|| {
		ProcessError::new(ProcessErrorKind::DecodeLimit, "CMYK JPEG size overflow")
	})?;
	decoder.set_max_decoding_buffer_size(expected);

	// jpeg-decoder 0.3.2 は CMYK を反転し、YCCK は K を反転して返す。
	let mut cmyk = decoder.decode().map_err(|e| {
		ProcessError::new(
			ProcessErrorKind::Decode,
			format!("CMYK JPEG decode: {:?}", e),
		)
	})?;
	if cmyk.len() != expected {
		return Err(ProcessError::new(
			ProcessErrorKind::Decode,
			"Invalid CMYK buffer size",
		));
	}
	// Adobe CMYK / YCCK では上のデコーダ出力をそのまま使う。
	// APP14 のない通常 CMYK では、デコーダによる反転を戻す。
	if adobe.is_none() {
		for value in &mut cmyk {
			*value = 255 - *value;
		}
	}

	let srgb = lcms2::Profile::new_srgb();
	let transform: lcms2::Transform<[u8; 4], [u8; 3]> = lcms2::Transform::new(
		&input,
		lcms2::PixelFormat::CMYK_8,
		&srgb,
		lcms2::PixelFormat::RGB_8,
		lcms2::Intent::Perceptual,
	)
	.map_err(|e| {
		ProcessError::new(
			ProcessErrorKind::Decode,
			format!("CMYK ICC transform: {:?}", e),
		)
	})?;

	let mut rgb = image::RgbImage::new(width, height);
	let mut row_in = vec![[0u8; 4]; width as usize];
	let mut row_out = vec![[0u8; 3]; width as usize];
	for (y, row) in cmyk.chunks_exact(width as usize * 4).enumerate() {
		for (pixel, bytes) in row_in.iter_mut().zip(row.chunks_exact(4)) {
			pixel.copy_from_slice(bytes);
		}
		transform.transform_pixels(&row_in, &mut row_out);
		for (x, pixel) in row_out.iter().enumerate() {
			rgb.put_pixel(x as u32, y as u32, image::Rgb(*pixel));
		}
	}

	Ok(DynamicImage::ImageRgb8(rgb))
}

fn badge_entropy(img: &image::GrayImage) -> f32 {
	let mut hist = [0u64; 256];
	for p in img.pixels() {
		hist[p.0[0] as usize] += 1;
	}
	let total = (img.width() as f32) * (img.height() as f32);
	if total <= 0.0 {
		return 0.0;
	}
	let mut entropy = 0.0f32;
	for &count in &hist {
		if count == 0 {
			continue;
		}
		let p = count as f32 / total;
		entropy -= p * p.log2();
	}
	entropy
}

pub(crate) fn exif_rotate(src: &[u8], img: DynamicImage) -> DynamicImage {
	let exifreader = rexif::parse_buffer_quiet(src);
	if let Ok(exif) = exifreader.0 {
		for e in exif.entries {
			match e.tag {
				rexif::ExifTag::Orientation => {
					return match e.value.to_i64(0).unwrap_or(0) {
						2 => img.fliph(),
						3 => img.rotate180(),
						4 => img.flipv(),
						5 => img.rotate90().fliph(),
						6 => img.rotate90(),
						7 => img.rotate270().fliph(),
						8 => img.rotate270(),
						_ => img,
					};
				}
				_ => {}
			}
		}
	}
	img
}

fn jpegxr_img(
	width: u32,
	height: u32,
	stride: usize,
	buffer: Vec<u8>,
	info: jpegxr::PixelFormat,
) -> Option<DynamicImage> {
	match info {
		jpegxr::PixelFormat::PixelFormat8bppGray => {
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageLuma8(i))
		}
		jpegxr::PixelFormat::PixelFormat24bppBGR => {
			let mut buffer = buffer;
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 3;
					let r = buffer[offset];
					buffer[offset] = buffer[offset + 2];
					buffer[offset + 2] = r;
				}
			}
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat24bppRGB => {
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppBGR => {
			let mut raw_img = Vec::with_capacity(width as usize * height as usize * 3);
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 4;
					raw_img.push(buffer[offset + 2]);
					raw_img.push(buffer[offset + 1]);
					raw_img.push(buffer[offset + 0]);
				}
			}
			image::ImageBuffer::from_raw(width, height, raw_img).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppBGRA => {
			let mut buffer = buffer;
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 4;
					let r = buffer[offset];
					buffer[offset] = buffer[offset + 2];
					buffer[offset + 2] = r;
				}
			}
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgba8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppRGB => {
			let mut raw_img = Vec::with_capacity(height as usize * 3);
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 4;
					raw_img.push(buffer[offset + 0]);
					raw_img.push(buffer[offset + 1]);
					raw_img.push(buffer[offset + 2]);
				}
			}
			image::ImageBuffer::from_raw(width, height, raw_img).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppRGBA => {
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgba8(i))
		}
		_ => None,
	}
}

pub(crate) fn encode_jp2(
	src: &[u8],
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Result<Encoded, ProcessError> {
	let dims = jpeg2k::DumpImage::from_bytes(src)
		.ok()
		.map(|dump| (dump.img.width(), dump.img.height()));
	if let Some((w, h)) = dims {
		if !dimensions_allowed_for(cfg.max_decode_pixels, w as u64, h as u64) {
			return Err(ProcessError::new(
				ProcessErrorKind::DecodeLimit,
				format!("DecodeDimensions {}x{} over limit", w, h),
			));
		}
	}
	let img = jpeg2k::Image::from_bytes(src)
		.map_err(|e| ProcessError::new(ProcessErrorKind::Decode, format!("{:?}", e)))?;
	let img = DynamicImage::try_from(&img)
		.map_err(|e| ProcessError::new(ProcessErrorKind::Decode, format!("{:?}", e)))?;
	response_img(img, src, None, opts, cfg, None)
}

pub(crate) fn encode_jxr(
	src: &[u8],
	opts: &ProcessOptions,
	cfg: &ProcessorConfig,
) -> Result<Encoded, ProcessError> {
	fn decode_jxr(
		src_bytes: &[u8],
		max_pixels: u64,
	) -> Result<Result<DynamicImage, String>, jpegxr::JXRError> {
		use jpegxr::{ImageDecode, PixelInfo};
		let mut decoder = ImageDecode::with_reader(Cursor::new(src_bytes))?;
		let (width, height) = decoder.get_size()?;
		let (w, h) = (width as u64, height as u64);
		if !dimensions_allowed_for(max_pixels, w, h) {
			return Ok(Err(format!(
				"DecodeDimensions {}x{} over limit",
				width, height
			)));
		}
		let info = PixelInfo::from_format(decoder.get_pixel_format()?);
		let stride = width as usize * info.bits_per_pixel() as usize / 8;
		let size = stride * height as usize;
		let mut buffer = Vec::<u8>::with_capacity(size);
		buffer.resize(size, 0);
		decoder.alpha_mode(info.has_alpha());
		decoder.copy_all(&mut buffer, stride)?;
		let img = jpegxr_img(width as u32, height as u32, stride, buffer, info.format());
		Ok(img.ok_or_else(|| {
			format!(
				"color_format={:?}&bgr={}&channels={}&format={:?}",
				info.color_format(),
				info.bgr(),
				info.channels(),
				info.format()
			)
		}))
	}
	match decode_jxr(src, cfg.max_decode_pixels) {
		Ok(Ok(img)) => response_img(img, src, None, opts, cfg, None),
		Ok(Err(e)) => Err(ProcessError::new(
			ProcessErrorKind::DecodeLimit,
			format!("JpegXR decode pixels {:?}", e),
		)),
		Err(e) => Err(ProcessError::new(
			ProcessErrorKind::Decode,
			format!("JpegXR decode bytes {:?}", e),
		)),
	}
}

fn jxl_render_to_image(render: &jxl_oxide::Render) -> Option<DynamicImage> {
	let mut stream = render.stream();
	let width = stream.width();
	let height = stream.height();
	let channels = stream.channels() as usize;
	let len = (width as usize)
		.checked_mul(height as usize)?
		.checked_mul(channels)?;
	let mut buf = vec![0u8; len];
	stream.write_to_buffer(&mut buf);
	match channels {
		1 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageLuma8),
		2 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageLumaA8),
		3 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageRgb8),
		4 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageRgba8),
		_ => None,
	}
}

/// webpクレートは終端タイムスタンプを渡せず最終フレームが平均尺になるため、総尺から逆算して書き換える。
fn set_last_anmf_duration(buf: &mut [u8], total_ms: u32) {
	if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WEBP" {
		return;
	}
	let read24 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]);
	let mut pos = 12;
	let mut last: Option<usize> = None;
	let mut sum_before_last: u32 = 0;
	while pos + 8 <= buf.len() {
		let size = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
		let payload = pos + 8;
		if &buf[pos..pos + 4] == b"ANMF" && payload + 16 <= buf.len() {
			if let Some(prev) = last {
				sum_before_last = sum_before_last.saturating_add(read24(&buf[prev..prev + 3]));
			}
			last = Some(payload + 12);
		}
		pos = payload.saturating_add(size).saturating_add(size & 1);
	}
	if let Some(off) = last {
		if total_ms >= sum_before_last {
			let d = (total_ms - sum_before_last).min(0xFF_FFFF);
			buf[off..off + 3].copy_from_slice(&d.to_le_bytes()[..3]);
		}
	}
}
pub fn image_to_frame(
	image: &'_ DynamicImage,
	timestamp: i32,
) -> Result<webp::AnimFrame<'_>, &'static str> {
	match image {
		DynamicImage::ImageLuma8(_) => Err("Unimplemented"),
		DynamicImage::ImageLumaA8(_) => Err("Unimplemented"),
		DynamicImage::ImageRgb8(image) => Ok(webp::AnimFrame::from_rgb(
			image.as_ref(),
			image.width(),
			image.height(),
			timestamp,
		)),
		DynamicImage::ImageRgba8(image) => Ok(webp::AnimFrame::from_rgba(
			image.as_ref(),
			image.width(),
			image.height(),
			timestamp,
		)),
		_ => Err("Unimplemented"),
	}
}

fn resize_inner(
	img: DynamicImage,
	max_width: u32,
	max_height: u32,
	filter: fast_image_resize::FilterType,
) -> Option<DynamicImage> {
	let scale = f32::min(
		max_width as f32 / img.width() as f32,
		max_height as f32 / img.height() as f32,
	);
	let dst_width = 1.max((img.width() as f32 * scale).round() as u32);
	let dst_height = 1.max((img.height() as f32 * scale).round() as u32);
	let src_image = fast_image_resize::images::Image::from_vec_u8(
		img.width(),
		img.height(),
		img.into_rgba8().into_raw(),
		fast_image_resize::PixelType::U8x4,
	);
	let src_image = src_image.ok()?;
	let mut dst_image =
		fast_image_resize::images::Image::new(dst_width, dst_height, src_image.pixel_type());
	let mut resizer = fast_image_resize::Resizer::new();
	let options = fast_image_resize::ResizeOptions {
		algorithm: fast_image_resize::ResizeAlg::Convolution(filter),
		..Default::default()
	};
	if resizer
		.resize(&src_image, &mut dst_image, &options)
		.is_err()
	{
		return None;
	}
	let rgba =
		image::RgbaImage::from_raw(dst_image.width(), dst_image.height(), dst_image.into_vec());
	Some(DynamicImage::ImageRgba8(rgba?))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{FilterType, ProcessOptions, ProcessorConfig};

	fn png_chunk(ctype: &[u8; 4], data: &[u8]) -> Vec<u8> {
		let mut v = Vec::new();
		v.extend_from_slice(&(data.len() as u32).to_be_bytes());
		v.extend_from_slice(ctype);
		v.extend_from_slice(data);
		v.extend_from_slice(&[0, 0, 0, 0]); //CRCはpng_apng_within_budgetで検証されない
		v
	}
	fn build_apng(width: u32, height: u32, frames: u32) -> Vec<u8> {
		let mut v = vec![137, 80, 78, 71, 13, 10, 26, 10];
		let mut ihdr = Vec::new();
		ihdr.extend_from_slice(&width.to_be_bytes());
		ihdr.extend_from_slice(&height.to_be_bytes());
		ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
		v.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
		let mut actl = Vec::new();
		actl.extend_from_slice(&frames.to_be_bytes());
		actl.extend_from_slice(&0u32.to_be_bytes());
		v.extend_from_slice(&png_chunk(b"acTL", &actl));
		v
	}
	fn build_png_without_actl(width: u32, height: u32) -> Vec<u8> {
		let mut v = vec![137, 80, 78, 71, 13, 10, 26, 10];
		let mut ihdr = Vec::new();
		ihdr.extend_from_slice(&width.to_be_bytes());
		ihdr.extend_from_slice(&height.to_be_bytes());
		ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
		v.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
		v.extend_from_slice(&png_chunk(b"IDAT", &[0, 1, 2, 3]));
		v
	}

	#[test]
	fn apng_non_png_data_is_ok() {
		assert!(png_apng_within_budget(b"not a png at all", 1_000_000).is_ok());
	}
	#[test]
	fn apng_within_budget_is_ok() {
		let data = build_apng(10, 10, 5);
		assert!(png_apng_within_budget(&data, 10_000).is_ok());
	}
	#[test]
	fn apng_frames_at_limit_is_ok() {
		let data = build_apng(1, 1, ANIMATION_FRAMES_LIMIT as u32);
		assert!(png_apng_within_budget(&data, ANIMATION_FRAMES_LIMIT).is_ok());
	}
	#[test]
	fn apng_frames_over_limit_is_err() {
		let data = build_apng(1, 1, ANIMATION_FRAMES_LIMIT as u32 + 1);
		let err = png_apng_within_budget(&data, ANIMATION_FRAMES_LIMIT + 1).unwrap_err();
		assert!(err.contains("FramesLimit"), "{}", err);
	}
	#[test]
	fn apng_decode_pixels_over_budget_is_err() {
		let data = build_apng(100_000, 100_000, 2);
		let err = png_apng_within_budget(&data, 1_000).unwrap_err();
		assert!(err.contains("DecodePixels"), "{}", err);
	}
	#[test]
	fn png_without_actl_is_ok() {
		let data = build_png_without_actl(10, 10);
		assert!(png_apng_within_budget(&data, 1_000_000).is_ok());
	}
	#[test]
	fn apng_truncated_chunk_is_ok() {
		let mut data = vec![137, 80, 78, 71, 13, 10, 26, 10];
		data.extend_from_slice(&[0, 0, 0, 20]);
		data.extend_from_slice(b"IHDR");
		assert!(png_apng_within_budget(&data, 1_000_000).is_ok());
	}

	fn gif_frame(width: u16, height: u16) -> Vec<u8> {
		let mut v = vec![0x2C];
		v.extend_from_slice(&0u16.to_le_bytes()); //left
		v.extend_from_slice(&0u16.to_le_bytes()); //top
		v.extend_from_slice(&width.to_le_bytes());
		v.extend_from_slice(&height.to_le_bytes());
		v.push(0); //packed:LCT無し
		v.push(2); //LZW最小コードサイズ
		v.push(1); //サブブロック長
		v.push(0); //画像データ1バイト
		v.push(0); //ゼロ長ブロックで終端
		v
	}
	fn gif_ext() -> Vec<u8> {
		vec![0x21, 0xF9, 4, 0, 0, 0, 0, 0]
	}
	fn build_gif(canvas_w: u16, canvas_h: u16, frames: &[Vec<u8>]) -> Vec<u8> {
		let mut v = Vec::new();
		v.extend_from_slice(b"GIF89a");
		v.extend_from_slice(&canvas_w.to_le_bytes());
		v.extend_from_slice(&canvas_h.to_le_bytes());
		v.push(0); //packed:GCT無し
		v.push(0); //背景色インデックス
		v.push(0); //画素比
		for f in frames {
			v.extend_from_slice(f);
		}
		v.push(0x3B); //トレーラー
		v
	}

	#[test]
	fn gif_non_gif_data_is_ok() {
		assert!(gif_animation_within_budget(b"not a gif", 1_000_000).is_ok());
	}
	#[test]
	fn gif_single_frame_is_ok() {
		let data = build_gif(10, 10, &[gif_frame(10, 10)]);
		assert!(gif_animation_within_budget(&data, 1_000).is_ok());
	}
	#[test]
	fn gif_frames_at_limit_is_ok() {
		let frames: Vec<Vec<u8>> = (0..ANIMATION_FRAMES_LIMIT)
			.map(|_| gif_frame(1, 1))
			.collect();
		let data = build_gif(1, 1, &frames);
		assert!(gif_animation_within_budget(&data, ANIMATION_FRAMES_LIMIT).is_ok());
	}
	#[test]
	fn gif_frames_over_limit_is_err() {
		let frames: Vec<Vec<u8>> = (0..ANIMATION_FRAMES_LIMIT + 1)
			.map(|_| gif_frame(1, 1))
			.collect();
		let data = build_gif(1, 1, &frames);
		let err = gif_animation_within_budget(&data, ANIMATION_FRAMES_LIMIT + 1).unwrap_err();
		assert!(err.contains("FramesLimit"), "{}", err);
	}
	#[test]
	fn gif_decode_pixels_over_budget_is_err() {
		let data = build_gif(100, 100, &[gif_frame(100, 100)]);
		let err = gif_animation_within_budget(&data, 1_000).unwrap_err();
		assert!(err.contains("DecodePixels"), "{}", err);
	}
	#[test]
	fn gif_frame_rect_exceeding_screen_is_err() {
		let data = build_gif(1, 1, &[gif_frame(65535, 65535)]);
		let err = gif_animation_within_budget(&data, 1_000).unwrap_err();
		assert!(err.contains("FrameRect"), "{}", err);
	}
	#[test]
	fn gif_extension_blocks_are_not_counted_as_frames() {
		let mut frames: Vec<Vec<u8>> = (0..ANIMATION_FRAMES_LIMIT + 1).map(|_| gif_ext()).collect();
		frames.push(gif_frame(10, 10));
		let data = build_gif(10, 10, &frames);
		assert!(gif_animation_within_budget(&data, 1_000).is_ok());
	}

	fn anmf_durations(buf: &[u8]) -> Vec<u32> {
		let mut v = vec![];
		let mut pos = 12;
		while pos + 8 <= buf.len() {
			let size = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
			if &buf[pos..pos + 4] == b"ANMF" {
				let d = pos + 8 + 12;
				v.push(u32::from_le_bytes([buf[d], buf[d + 1], buf[d + 2], 0]));
			}
			pos += 8 + size + (size & 1);
		}
		v
	}
	#[test]
	fn anim_webp_keeps_per_frame_durations() {
		let conf = webp::WebPConfig::new().unwrap();
		let colors = [[255u8, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
		let imgs: Vec<DynamicImage> = colors
			.iter()
			.map(|c| DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba(*c))))
			.collect();
		let delays: [u32; 3] = [100, 800, 700];
		let mut enc = webp::AnimEncoder::new(4, 4, &conf);
		let mut ts = 0;
		for (img, d) in imgs.iter().zip(delays) {
			enc.add_frame(image_to_frame(img, ts as i32).unwrap())
				.unwrap();
			ts += d;
		}
		let mut buf = enc.encode();
		set_last_anmf_duration(&mut buf, ts);
		assert_eq!(anmf_durations(&buf), delays);
	}

	fn test_config_opts(max_size: u64) -> (ProcessorConfig, ProcessOptions) {
		let cfg = ProcessorConfig {
			max_pixels: 2048,
			max_decode_pixels: max_size / 4,
			filter_type: FilterType::Triangle,
			webp_quality: 75.0,
			encode_avif: false,
			load_system_fonts: false,
			font_dirs: Vec::new(),
			default_font_family: None,
		};
		let opts = ProcessOptions::default();
		(cfg, opts)
	}
	fn make_frame() -> Result<image::Frame, image::ImageError> {
		let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
		Ok(image::Frame::from_parts(
			img,
			0,
			0,
			image::Delay::from_saturating_duration(Duration::from_millis(10)),
		))
	}

	#[test]
	fn encode_anim_allows_exactly_frame_limit() {
		let frames: Vec<_> = (0..ANIMATION_FRAMES_LIMIT).map(|_| make_frame()).collect();
		let frames = image::Frames::new(Box::new(frames.into_iter()));
		let (cfg, opts) = test_config_opts(1_000_000);
		let resp = encode_anim(frames, 0, &opts, &cfg, None);
		assert!(resp.is_ok(), "{:?}", resp);
	}
	#[test]
	fn encode_anim_rejects_over_frame_limit() {
		let frames: Vec<_> = (0..ANIMATION_FRAMES_LIMIT + 1)
			.map(|_| make_frame())
			.collect();
		let frames = image::Frames::new(Box::new(frames.into_iter()));
		let (cfg, opts) = test_config_opts(1_000_000);
		let err = encode_anim(frames, 0, &opts, &cfg, None).unwrap_err();
		assert_eq!(err.kind, ProcessErrorKind::DecodeLimit);
		assert!(err
			.message
			.contains(&format!("FramesLimit {}", ANIMATION_FRAMES_LIMIT)));
	}

	/// 出力 WebP の ANIM チャンクから loop_count を読む。
	/// payload は背景色4バイト + loop_count(u16 LE) の計6バイト。
	fn webp_anim_loop_count(buf: &[u8]) -> u32 {
		assert_eq!(&buf[0..4], b"RIFF");
		assert_eq!(&buf[8..12], b"WEBP");
		let mut pos = 12;
		while pos + 8 <= buf.len() {
			let size = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
			if &buf[pos..pos + 4] == b"ANIM" {
				let payload = &buf[pos + 8..pos + 8 + size];
				assert!(payload.len() >= 6, "short ANIM payload");
				return u16::from_le_bytes([payload[4], payload[5]]) as u32;
			}
			pos += 8 + size + (size & 1);
		}
		panic!("ANIM chunk not found");
	}

	fn build_apng_with_plays(num_plays: u32) -> Vec<u8> {
		let mut out = Vec::new();
		{
			let mut enc = png::Encoder::new(Cursor::new(&mut out), 2, 2);
			enc.set_color(png::ColorType::Rgba);
			enc.set_depth(png::BitDepth::Eight);
			enc.set_animated(2, num_plays).unwrap();
			let mut writer = enc.write_header().unwrap();
			writer.set_frame_delay(1, 10).unwrap();
			writer
				.write_image_data(&[255, 0, 0, 255].repeat(4))
				.unwrap();
			writer.set_frame_delay(1, 10).unwrap();
			writer
				.write_image_data(&[0, 255, 0, 255].repeat(4))
				.unwrap();
			writer.finish().unwrap();
		}
		out
	}

	#[test]
	fn apng_finite_loop_count_is_preserved() {
		let src = build_apng_with_plays(3);
		let (cfg, opts) = test_config_opts(1_000_000);
		let resp = encode_raster(&src, &opts, &cfg, image::ImageFormat::Png).unwrap();
		assert_eq!(webp_anim_loop_count(&resp.bytes), 3);
	}

	#[test]
	fn apng_infinite_loop_count_stays_infinite() {
		let src = build_apng_with_plays(0);
		let (cfg, opts) = test_config_opts(1_000_000);
		let resp = encode_raster(&src, &opts, &cfg, image::ImageFormat::Png).unwrap();
		assert_eq!(webp_anim_loop_count(&resp.bytes), 0);
	}

	fn build_gif_with_repeat(repeat: image::codecs::gif::Repeat) -> Vec<u8> {
		let mut out = Vec::new();
		{
			let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
			enc.set_repeat(repeat).unwrap();
			for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
				let img = image::RgbaImage::from_pixel(2, 2, image::Rgba(color));
				let frame = image::Frame::from_parts(
					img,
					0,
					0,
					image::Delay::from_saturating_duration(Duration::from_millis(10)),
				);
				enc.encode_frame(frame).unwrap();
			}
		}
		out
	}

	#[test]
	fn gif_finite_loop_count_is_preserved() {
		let src = build_gif_with_repeat(image::codecs::gif::Repeat::Finite(3));
		let (cfg, opts) = test_config_opts(1_000_000);
		let resp = encode_raster(&src, &opts, &cfg, image::ImageFormat::Gif).unwrap();
		assert_eq!(webp_anim_loop_count(&resp.bytes), 3);
	}

	#[test]
	fn gif_infinite_loop_count_stays_infinite() {
		let src = build_gif_with_repeat(image::codecs::gif::Repeat::Infinite);
		let (cfg, opts) = test_config_opts(1_000_000);
		let resp = encode_raster(&src, &opts, &cfg, image::ImageFormat::Gif).unwrap();
		assert_eq!(webp_anim_loop_count(&resp.bytes), 0);
	}

	#[test]
	fn apng_loop_count_over_webp_limit_is_clamped() {
		// WebP の loop_count は u16 のため、libwebp が拒否して panic しないこと。
		let src = build_apng_with_plays(u16::MAX as u32 + 1);
		let (cfg, opts) = test_config_opts(1_000_000);
		let resp = encode_raster(&src, &opts, &cfg, image::ImageFormat::Png).unwrap();
		assert_eq!(webp_anim_loop_count(&resp.bytes), u16::MAX as u32);
	}

	fn tiff_with_orientation(img: &image::GrayImage, orientation: u16) -> Vec<u8> {
		let width = img.width();
		let height = img.height();
		let strip_byte_counts = (width * height) as usize;
		let ifd_size = 2 + 10 * 12 + 4;
		let data_offset = 8 + ifd_size;
		let mut out = Vec::new();
		out.extend_from_slice(b"II");
		out.extend_from_slice(&42u16.to_le_bytes());
		out.extend_from_slice(&8u32.to_le_bytes());
		out.extend_from_slice(&10u16.to_le_bytes());
		let entries = [
			(256u16, 3u16, 1u32, width as u32),             // ImageWidth
			(257u16, 3u16, 1u32, height as u32),            // ImageLength
			(258u16, 3u16, 1u32, 8u32),                     // BitsPerSample
			(259u16, 3u16, 1u32, 1u32),                     // Compression
			(262u16, 3u16, 1u32, 1u32),                     // PhotometricInterpretation
			(273u16, 4u16, 1u32, data_offset as u32),       // StripOffsets
			(274u16, 3u16, 1u32, orientation as u32),       // Orientation
			(277u16, 3u16, 1u32, 1u32),                     // SamplesPerPixel
			(278u16, 4u16, 1u32, height as u32),            // RowsPerStrip
			(279u16, 4u16, 1u32, strip_byte_counts as u32), // StripByteCounts
		];
		for (tag, typ, count, value) in entries {
			out.extend_from_slice(&tag.to_le_bytes());
			out.extend_from_slice(&typ.to_le_bytes());
			out.extend_from_slice(&count.to_le_bytes());
			out.extend_from_slice(&value.to_le_bytes());
		}
		out.extend_from_slice(&0u32.to_le_bytes());
		out.extend_from_slice(img.as_raw());
		out
	}

	#[test]
	fn exif_rotate_preserves_gray_pixels() {
		let img = image::GrayImage::from_pixel(8, 8, image::Luma([123]));
		let src = tiff_with_orientation(&img, 6);
		let rotated = exif_rotate(&src, DynamicImage::ImageLuma8(img.clone()));
		assert!(matches!(rotated, DynamicImage::ImageLuma8(_)));
		// Orientation=6 (rotate90) swaps width and height.
		assert_eq!(rotated.dimensions(), (8, 8));
	}
}
