use web_audio_api::{
    worklet::{AudioParamValues, AudioWorkletGlobalScope, AudioWorkletProcessor},
    AudioParamDescriptor,
};
use webrtc_audio_processing::NUM_SAMPLES_PER_FRAME;

use crate::audio::Processor;

use super::Interleave;

pub enum Direction {
    Capture,
    Render,
}
pub struct WebrtcProcessor {
    processor: Processor,
    direction: Direction,
    unprocessed: Vec<f32>,
    processed: Vec<f32>,
}

impl AudioWorkletProcessor for WebrtcProcessor {
    type ProcessorOptions = (Processor, Direction);

    fn constructor(opts: Self::ProcessorOptions) -> Self {
        let (processor, direction) = opts;
        Self {
            processor,
            direction,
            unprocessed: Vec::new(),
            processed: Vec::new(),
        }
    }

    fn parameter_descriptors() -> Vec<AudioParamDescriptor>
    where
        Self: Sized,
    {
        vec![]
    }

    fn process<'a, 'b>(
        &mut self,
        inputs: &'b [&'a [&'a [f32]]],
        outputs: &'b mut [&'a mut [&'a mut [f32]]],
        _params: AudioParamValues<'b>,
        _scope: &'b AudioWorkletGlobalScope,
    ) -> bool {
        let channel_count = inputs.len();
        let input_interleaved = inputs[0].iter().zip(inputs[1].iter()).flat_map(|(a, b)| {
            a.iter()
                .zip(b.iter())
                .flat_map(|(a, b)| [*a, *b].into_iter())
        });

        let output_len = outputs
            .iter()
            .flat_map(|output| output.iter().flat_map(|x| x.iter()))
            .count();

        let (output_0, output_1) = outputs.split_at_mut(1);
        let output_0 = &mut output_0[0];
        let output_1 = &mut output_1[0];
        let output_interleaved = output_0
            .iter_mut()
            .zip(output_1.iter_mut())
            .flat_map(|(a, b)| {
                a.iter_mut()
                    .zip(b.iter_mut())
                    .flat_map(|(a, b)| [a, b].into_iter())
            });

        self.unprocessed.extend(input_interleaved);

        // process
        let len = NUM_SAMPLES_PER_FRAME as usize * channel_count;
        if self.unprocessed.len() >= len {
            match self.direction {
                Direction::Capture => self
                    .processor
                    .process_capture_frame(&mut self.unprocessed[..len])
                    .unwrap(),
                Direction::Render => self
                    .processor
                    .process_render_frame(&mut self.unprocessed[..len])
                    .unwrap(),
            };
            self.processed.extend(&self.unprocessed[..len]);
            self.unprocessed.copy_within(len.., 0);
            self.unprocessed.truncate(self.unprocessed.len() - len)
        }
        if self.processed.len() > output_len {
            for (i, sample) in output_interleaved.enumerate() {
                *sample = self.processed[i];
            }
            self.processed.copy_within(output_len.., 0);
            self.processed.truncate(self.processed.len() - output_len);
        } else {
            tracing::warn!(
                "processor underflow: output_len {output_len} processed {} unprocessed {}",
                self.processed.len(),
                self.unprocessed.len()
            );
            for sample in output_interleaved {
                *sample = 0.
            }
        }

        false
    }
}
