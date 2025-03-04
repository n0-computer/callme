use std::{io::Cursor, time::Duration};

use cpal::{
    traits::{DeviceTrait, StreamTrait},
    Sample,
};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use tracing::{trace, warn};

use super::{
    device::{find_device, input_stream_config, output_stream_config, Direction},
    processor::Processor,
    DURATION_10MS, DURATION_20MS, OPUS_STREAM_PARAMS,
};

pub fn feedback() -> anyhow::Result<()> {
    let host = cpal::default_host();
    let input_device = find_device(&host, Direction::Input, None)?;
    let output_device = find_device(&host, Direction::Output, None)?;

    let params = OPUS_STREAM_PARAMS;
    let input_info = input_stream_config(&input_device, &params)?;
    let output_info = output_stream_config(&output_device, &params)?;

    let buffer_size = params.buffer_size(DURATION_20MS) * 16;
    let (mut producer, mut consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();

    let processor = Processor::new(1, 1, None)?;
    let processor_clone = processor.clone();

    let mut input_buf = Vec::with_capacity(params.buffer_size(DURATION_10MS));
    let frame_size = params.buffer_size(DURATION_10MS);
    let input_stream = input_device.build_input_stream(
        &input_info.config,
        move |data: &[f32], _info: &_| {
            for s in data {
                input_buf.push(*s);
                if input_buf.len() == frame_size {
                    processor.process_capture_frame(&mut input_buf[..]).unwrap();
                    let n = producer.push_slice(&input_buf);
                    if n < data.len() {
                        warn!(
                            "record overflow: failed to push {} of {}",
                            data.len() - n,
                            data.len()
                        );
                    }
                    input_buf.clear();
                }
            }
        },
        |err| warn!("input err {err:?}"),
        None,
    )?;

    let processor = processor_clone;
    let mut unprocessed: Vec<f32> = Vec::with_capacity(params.buffer_size(DURATION_10MS));
    let mut processed: Vec<f32> = Vec::with_capacity(params.buffer_size(DURATION_10MS));
    // let buf_size = frame_size;
    let output_stream = input_device.build_output_stream(
        &output_info.config,
        move |data: &mut [f32], _info: &_| {
            // pop data and process, if possible
            unprocessed.extend(consumer.pop_iter().take(frame_size - unprocessed.len()));
            if unprocessed.len() == frame_size {
                processor.process_render_frame(&mut unprocessed).unwrap();
                processed.extend(&unprocessed);
                unprocessed.clear();
            }

            // copy to out
            let out_len = processed.len().min(data.len());
            let processed_remaining = processed.len() - out_len;
            data[..out_len].copy_from_slice(&processed[..out_len]);
            processed.copy_within(out_len.., 0);
            processed.truncate(processed_remaining);
            if out_len < data.len() {
                warn!(
                    "playback underflow: {} of {} samples missing (buffered {})",
                    data.len() - out_len,
                    data.len(),
                    unprocessed.len() + consumer.occupied_len(),
                );
            }
        },
        |err| warn!("output err {err:?}"),
        None,
    )?;
    output_stream.play()?;
    input_stream.play()?;

    std::thread::sleep(Duration::from_secs(5));

    Ok(())
}

// trace!(
//     "played {}. processed={} unprocessed={} ringbuf={} frame_size={}",
//     out_len,
//     processed.len(),
//     unprocessed.len(),
//     consumer.occupied_len(),
//     frame_size
// );

// trace!(
//                 "playing {}. processed={} unprocessed={} ringbuf={} frame_size={}",
//                 data.len(),
//                 processed.len(),
//                 unprocessed.len(),
//                 consumer.occupied_len(),
//                 frame_size
//             );
