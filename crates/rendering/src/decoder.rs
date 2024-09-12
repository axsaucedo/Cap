use std::{
    cell::Cell,
    collections::BTreeMap,
    path::PathBuf,
    ptr::{null, null_mut},
    slice,
    sync::{mpsc, Arc},
};

use ffmpeg_next::{
    codec,
    format::{self, context::input::PacketIter, Pixel},
    frame::{self, Video},
    rescale, Codec, Packet, Rational, Rescale, Stream,
};
use ffmpeg_sys_next::{
    av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, av_hwframe_transfer_data,
    avcodec_find_decoder, avcodec_get_hw_config, AVBufferRef, AVCodecContext, AVHWDeviceType,
    AVPixelFormat, AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX,
};

pub type DecodedFrame = Arc<Vec<u8>>;

enum VideoDecoderMessage {
    GetFrame(u32, tokio::sync::oneshot::Sender<Option<Arc<Vec<u8>>>>),
}

fn ts_to_frame(ts: i64, time_base: Rational, frame_rate: Rational) -> u32 {
    // dbg!((ts, time_base, frame_rate));
    ((ts * time_base.numerator() as i64 * frame_rate.numerator() as i64)
        / (time_base.denominator() as i64 * frame_rate.denominator() as i64)) as u32
}

const FRAME_CACHE_SIZE: usize = 50;

pub struct AsyncVideoDecoder;

impl AsyncVideoDecoder {
    pub fn spawn(path: PathBuf) -> AsyncVideoDecoderHandle {
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let mut input = ffmpeg_next::format::input(&path).unwrap();

            let input_stream = input
                .streams()
                .best(ffmpeg_next::media::Type::Video)
                .ok_or("Could not find a video stream")
                .unwrap();

            let decoder_codec =
                ff_find_decoder(&input, &input_stream, input_stream.parameters().id()).unwrap();

            let mut context = codec::context::Context::new_with_codec(decoder_codec);
            context.set_parameters(input_stream.parameters()).unwrap();

            let hw_device = {
                #[cfg(target_os = "macos")]
                {
                    context
                        .try_use_hw_device(
                            AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                            Pixel::NV12,
                        )
                        .ok()
                }

                #[cfg(not(target_os = "macos"))]
                None
            };

            let input_stream_index = input_stream.index();
            let time_base = input_stream.time_base();
            let frame_rate = input_stream.rate();

            // Create a decoder for the video stream
            let mut decoder = context.decoder().video().unwrap();

            use ffmpeg_next::format::Pixel;
            use ffmpeg_next::software::scaling::{context::Context, flag::Flags};

            let mut scaler_input_format = hw_device
                .as_ref()
                .map(|d| d.pix_fmt)
                .unwrap_or(decoder.format());

            let mut scaler = Context::get(
                scaler_input_format,
                decoder.width(),
                decoder.height(),
                Pixel::RGBA,
                decoder.width(),
                decoder.height(),
                Flags::BILINEAR,
            )
            .unwrap();

            let mut temp_frame = ffmpeg_next::frame::Video::empty();

            let render_more_margin = (FRAME_CACHE_SIZE / 4) as u32;

            let mut cache = BTreeMap::<u32, Arc<Vec<u8>>>::new();
            // active frame is a frame that triggered decode.
            // frames that are within render_more_margin of this frame won't trigger decode.
            let mut last_active_frame = None::<u32>;

            let mut last_decoded_frame = None::<u32>;

            struct PacketStuff<'a> {
                packets: PacketIter<'a>,
                skipped_packet: Option<(Stream<'a>, Packet)>,
            }

            let mut peekable_requests = PeekableReceiver { rx, peeked: None };

            let mut packet_stuff = PacketStuff {
                packets: input.packets(),
                skipped_packet: None,
            };

            while let Ok(r) = peekable_requests.recv() {
                match r {
                    VideoDecoderMessage::GetFrame(frame_number, sender) => {
                        // println!("received request for frame {frame_number}");

                        let mut sender = if let Some(cached) = cache.get(&frame_number) {
                            sender.send(Some(cached.clone())).ok();
                            continue;
                        } else {
                            Some(sender)
                        };

                        let cache_min = frame_number.saturating_sub(FRAME_CACHE_SIZE as u32 / 2);
                        let cache_max = frame_number + FRAME_CACHE_SIZE as u32 / 2;

                        if frame_number <= 0
                            || last_decoded_frame
                                .map(|f| {
                                    frame_number < f ||
                                    // seek forward for big jumps. this threshold is arbitrary but should be derived from i-frames in future
                                    frame_number - f > FRAME_CACHE_SIZE as u32
                                })
                                .unwrap_or(true)
                        {
                            let timestamp_us =
                                ((frame_number as f32 / frame_rate.numerator() as f32)
                                    * 1_000_000.0) as i64;
                            let position = timestamp_us.rescale((1, 1_000_000), rescale::TIME_BASE);

                            // println!("seeking to {position} for frame {frame_number}");

                            drop(packet_stuff);

                            decoder.flush();
                            input.seek(position, ..position).unwrap();
                            cache.clear();
                            last_decoded_frame = None;

                            packet_stuff = PacketStuff {
                                packets: input.packets(),
                                skipped_packet: None,
                            };
                        }

                        last_active_frame = Some(frame_number);

                        loop {
                            let Some((stream, packet)) = packet_stuff
                                .skipped_packet
                                .take()
                                .or_else(|| packet_stuff.packets.next())
                            else {
                                break;
                            };

                            let current_frame =
                                ts_to_frame(packet.pts().unwrap(), time_base, frame_rate);

                            let too_great_for_cache_bounds = current_frame > cache_max;
                            let too_small_for_cache_bounds = current_frame < cache_min;

                            if peekable_requests.peek().is_some() {
                                // println!("skipping packet for frame {current_frame} as new request is available");
                                packet_stuff.skipped_packet = Some((stream, packet));

                                break;
                            }

                            if stream.index() == input_stream_index {
                                if too_great_for_cache_bounds {
                                    // println!("skipping packet for frame {current_frame} as it's out of cache bounds");
                                    packet_stuff.skipped_packet = Some((stream, packet));
                                    break;
                                }

                                decoder.send_packet(&packet).unwrap();

                                last_decoded_frame = Some(current_frame);

                                while decoder.receive_frame(&mut temp_frame).is_ok() {
                                    // println!(
                                    //     "decoded frame {current_frame}. will cache: {}",
                                    //     !too_great_for_cache_bounds && !too_small_for_cache_bounds
                                    // );

                                    let sw_frame = try_transfer_hwframe(&temp_frame);
                                    let frame = sw_frame.as_ref().unwrap_or(&temp_frame);

                                    if frame.format() != scaler_input_format {
                                        // Reinitialize the scaler with the new input format
                                        scaler_input_format = frame.format();
                                        scaler = Context::get(
                                            scaler_input_format,
                                            decoder.width(),
                                            decoder.height(),
                                            Pixel::RGBA,
                                            decoder.width(),
                                            decoder.height(),
                                            Flags::BILINEAR,
                                        )
                                        .unwrap();
                                    }

                                    let mut rgb_frame = frame::Video::empty();
                                    scaler.run(frame, &mut rgb_frame).unwrap();

                                    let width = rgb_frame.width() as usize;
                                    let height = rgb_frame.height() as usize;
                                    let stride = rgb_frame.stride(0) as usize;
                                    let data = rgb_frame.data(0);

                                    let expected_size = width * height * 4;

                                    let mut frame_buffer = Vec::with_capacity(expected_size);

                                    let data_ptr = data.as_ptr();

                                    for y in 0..height {
                                        let line_start = unsafe { data_ptr.add(y * stride) };
                                        let line =
                                            unsafe { slice::from_raw_parts(line_start, width * 4) };
                                        frame_buffer.extend_from_slice(line);
                                    }

                                    let frame = Arc::new(frame_buffer);

                                    if current_frame == frame_number {
                                        if let Some(sender) = sender.take() {
                                            sender.send(Some(frame.clone())).ok();
                                        }
                                    }

                                    if !too_small_for_cache_bounds && !too_great_for_cache_bounds {
                                        if cache.len() >= FRAME_CACHE_SIZE {
                                            if let Some(last_active_frame) = &last_active_frame {
                                                let frame = if frame_number > *last_active_frame {
                                                    *cache.keys().next().unwrap()
                                                } else if frame_number < *last_active_frame {
                                                    *cache.keys().next_back().unwrap()
                                                } else {
                                                    let min = *cache.keys().min().unwrap();
                                                    let max = *cache.keys().max().unwrap();

                                                    if current_frame > max {
                                                        min
                                                    } else {
                                                        max
                                                    }
                                                };

                                                // println!("removing frame {frame} from cache");
                                                cache.remove(&frame);
                                            } else {
                                                // println!("clearing cache");
                                                cache.clear()
                                            }
                                        }

                                        // println!("caching frame {current_frame}");
                                        cache.insert(current_frame, frame);
                                    }
                                }
                            }
                        }

                        if sender.is_some() {
                            println!("failed to send frame {frame_number}");
                        }
                    }
                }
            }
        });

        AsyncVideoDecoderHandle { sender: tx }
    }
}

#[derive(Clone)]
pub struct AsyncVideoDecoderHandle {
    sender: mpsc::Sender<VideoDecoderMessage>,
}

impl AsyncVideoDecoderHandle {
    pub async fn get_frame(&self, frame_number: u32) -> Option<Arc<Vec<u8>>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(VideoDecoderMessage::GetFrame(frame_number, tx))
            .unwrap();
        rx.await.ok().flatten()
    }
}

struct PeekableReceiver<T> {
    rx: mpsc::Receiver<T>,
    peeked: Option<T>,
}

impl<T> PeekableReceiver<T> {
    fn peek(&mut self) -> Option<&T> {
        if self.peeked.is_some() {
            self.peeked.as_ref()
        } else {
            match self.rx.try_recv() {
                Ok(value) => {
                    self.peeked = Some(value);
                    self.peeked.as_ref()
                }
                Err(_) => None,
            }
        }
    }

    fn try_recv(&mut self) -> Result<T, mpsc::TryRecvError> {
        println!("try_recv");
        if let Some(value) = self.peeked.take() {
            Ok(value)
        } else {
            self.rx.try_recv()
        }
    }

    fn recv(&mut self) -> Result<T, mpsc::RecvError> {
        if let Some(value) = self.peeked.take() {
            Ok(value)
        } else {
            self.rx.recv()
        }
    }
}

thread_local! {
    static HW_PIX_FMT: Cell<AVPixelFormat> = const { Cell::new(AVPixelFormat::AV_PIX_FMT_NONE) };
}

unsafe extern "C" fn get_format(
    _: *mut AVCodecContext,
    pix_fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    let mut fmt = pix_fmts;

    loop {
        if *fmt == AVPixelFormat::AV_PIX_FMT_NONE {
            break;
        }

        if *fmt == HW_PIX_FMT.get() {
            return *fmt;
        }

        fmt = fmt.offset(1);
    }

    AVPixelFormat::AV_PIX_FMT_NONE
}

fn ff_find_decoder(
    s: &format::context::Input,
    st: &format::stream::Stream,
    codec_id: codec::Id,
) -> Option<Codec> {
    unsafe {
        use ffmpeg_next::media::Type;
        let codec = match st.parameters().medium() {
            Type::Video => Some((*s.as_ptr()).video_codec),
            Type::Audio => Some((*s.as_ptr()).audio_codec),
            Type::Subtitle => Some((*s.as_ptr()).subtitle_codec),
            _ => None,
        };

        if let Some(codec) = codec {
            if !codec.is_null() {
                return Some(Codec::wrap(codec));
            }
        }

        let found = avcodec_find_decoder(codec_id.into());

        if found.is_null() {
            return None;
        }
        Some(Codec::wrap(found))
    }
}

fn try_transfer_hwframe(src: &Video) -> Option<Video> {
    unsafe {
        if src.format() == HW_PIX_FMT.get().into() {
            let mut sw_frame = frame::Video::empty();

            if av_hwframe_transfer_data(sw_frame.as_mut_ptr(), src.as_ptr(), 0) >= 0 {
                return Some(sw_frame);
            };
        }
    }

    None
}

struct HwDevice {
    pub device_type: AVHWDeviceType,
    pub pix_fmt: Pixel,
    ctx: *mut AVBufferRef,
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.ctx);
        }
    }
}

trait CodecContextExt {
    fn try_use_hw_device(
        &mut self,
        device_type: AVHWDeviceType,
        pix_fmt: Pixel,
    ) -> Result<HwDevice, &'static str>;
}

impl CodecContextExt for codec::context::Context {
    fn try_use_hw_device(
        &mut self,
        device_type: AVHWDeviceType,
        pix_fmt: Pixel,
    ) -> Result<HwDevice, &'static str> {
        let codec = self.codec().ok_or("no codec")?;

        unsafe {
            let mut i = 0;
            loop {
                let config = avcodec_get_hw_config(codec.as_ptr(), i);
                if config.is_null() {
                    return Err("no hw config");
                }

                if (*config).methods & (AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) == 1
                    && (*config).device_type == AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX
                {
                    HW_PIX_FMT.set((*config).pix_fmt);
                    break;
                }

                i += 1;
            }

            let context = self.as_mut_ptr();

            (*context).get_format = Some(get_format);

            let mut hw_device_ctx = null_mut();

            if av_hwdevice_ctx_create(&mut hw_device_ctx, device_type, null(), null_mut(), 0) < 0 {
                return Err("failed to create hw device context");
            }

            (*context).hw_device_ctx = av_buffer_ref(hw_device_ctx);

            Ok(HwDevice {
                device_type,
                ctx: hw_device_ctx,
                pix_fmt,
            })
        }
    }
}
