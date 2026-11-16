// Vendored from nnnoiseless 0.5.2 (BSD-3-Clause, see LICENSE-RNNOISE). Patched: the model
// is read once into owned `Vec<i8>` layers by a safe byte conversion (no `unsafe` slice
// cast), only the built-in model exists, the state no longer borrows the model, the
// input-size `assert!` is a type (`[f32; 42]`), and `reset` is added. See README.md.

// Upstream style is kept so the copy stays diffable (README.md); these lints are style only.
#![allow(
    clippy::identity_op,
    clippy::manual_div_ceil,
    clippy::needless_borrow,
    clippy::needless_range_loop,
    clippy::neg_cmp_op_on_partial_ord,
    clippy::redundant_slicing
)]

use crate::util::{relu, sigmoid_approx, tansig_approx, zip3};

const MAX_NEURONS: usize = 128;

type Weights = Vec<i8>;

#[derive(Clone, Copy, Debug)]
pub enum Activation {
    Tanh = 0,
    Sigmoid = 1,
    Relu = 2,
}

const WEIGHTS_SCALE: f32 = 1.0 / 256.0;

#[derive(Clone)]
pub struct DenseLayer {
    /// An array of length `nb_neurons`.
    pub bias: Weights,
    /// An array of length `nb_inputs * nb_neurons`.
    pub input_weights: Weights,
    pub nb_inputs: usize,
    pub nb_neurons: usize,
    pub activation: Activation,
}

#[derive(Clone)]
pub struct GruLayer {
    /// An array of length `3 * nb_neurons`.
    pub bias: Weights,
    /// An array of length `3 * nb_inputs * nb_neurons`.
    pub input_weights: Weights,
    /// An array of length `3 * nb_neurons^2`.
    pub recurrent_weights: Weights,
    pub nb_inputs: usize,
    pub nb_neurons: usize,
    pub activation: Activation,
}

/// All the model parameters of the denoising network.
pub(crate) struct RnnModel {
    pub(crate) input_dense: DenseLayer,
    pub(crate) vad_gru: GruLayer,
    pub(crate) noise_gru: GruLayer,
    pub(crate) denoise_gru: GruLayer,
    pub(crate) denoise_output: DenseLayer,
    pub(crate) vad_output: DenseLayer,
}

pub(crate) struct RnnState {
    vad_gru_state: Vec<f32>,
    noise_gru_state: Vec<f32>,
    denoise_gru_state: Vec<f32>,
}

impl RnnModel {
    /// The built-in model (`weights.rnn`), or `None` if it does not parse.
    pub(crate) fn builtin() -> Option<RnnModel> {
        let bytes: &'static [u8] = include_bytes!("weights.rnn");
        let signed: Vec<i8> = bytes.iter().map(|&b| i8::from_ne_bytes([b])).collect();
        RnnModel::from_bytes_impl(&signed)
    }

    /// Reads a model in the nnnoiseless format.
    ///
    /// The format is simple: each NN layer is represented by an array of signed `i8`'s,
    /// and these layers as simply concatenated.
    fn from_bytes_impl(bytes: &[i8]) -> Option<RnnModel> {
        fn read_array(bytes: &[i8], len: usize) -> Option<(Weights, &[i8])> {
            if bytes.len() >= len {
                Some((bytes[..len].to_vec(), &bytes[len..]))
            } else {
                None
            }
        }

        fn unsigned(b: i8) -> Option<usize> {
            if b >= 0 { Some(b as usize) } else { None }
        }

        fn act(x: i8) -> Option<Activation> {
            match x {
                0 => Some(Activation::Tanh),
                1 => Some(Activation::Sigmoid),
                2 => Some(Activation::Relu),
                _ => None,
            }
        }

        fn read_dense(bytes: &[i8]) -> Option<(DenseLayer, &[i8])> {
            if bytes.len() < 3 {
                return None;
            }

            let nb_inputs = unsigned(bytes[0])?;
            let nb_neurons = unsigned(bytes[1])?;
            let activation = act(bytes[2])?;
            let (input_weights, bytes) = read_array(&bytes[3..], nb_neurons * nb_inputs)?;
            let (bias, bytes) = read_array(bytes, nb_neurons)?;

            let layer = DenseLayer {
                nb_inputs,
                nb_neurons,
                input_weights,
                bias,
                activation,
            };
            Some((layer, bytes))
        }

        fn read_gru(bytes: &[i8]) -> Option<(GruLayer, &[i8])> {
            if bytes.len() < 3 {
                return None;
            }

            let nb_inputs = unsigned(bytes[0])?;
            let nb_neurons = unsigned(bytes[1])?;
            let activation = act(bytes[2])?;
            let (input_weights, bytes) = read_array(&bytes[3..], 3 * nb_neurons * nb_inputs)?;
            let (recurrent_weights, bytes) = read_array(bytes, 3 * nb_neurons * nb_neurons)?;
            let (bias, bytes) = read_array(bytes, 3 * nb_neurons)?;

            let layer = GruLayer {
                nb_inputs,
                nb_neurons,
                input_weights,
                recurrent_weights,
                bias,
                activation,
            };
            Some((layer, bytes))
        }

        let (input_dense, bytes) = read_dense(bytes)?;
        let (vad_gru, bytes) = read_gru(bytes)?;
        let (noise_gru, bytes) = read_gru(bytes)?;
        let (denoise_gru, bytes) = read_gru(bytes)?;
        let (denoise_output, bytes) = read_dense(bytes)?;
        let (vad_output, bytes) = read_dense(bytes)?;

        if !bytes.is_empty() {
            return None;
        }

        // The input to the first layer must be of size 42, because that's how many features
        // there are. The denoise output must be of size 22, and the vad output must be of size 1.
        // Other than that, the output of one layer must match with the inputs of the following
        // layer.
        if input_dense.nb_inputs != 42
            || denoise_output.nb_neurons != 22
            || vad_output.nb_neurons != 1
        {
            return None;
        }
        if input_dense.nb_neurons != vad_gru.nb_inputs || vad_gru.nb_neurons != vad_output.nb_inputs
        {
            return None;
        }
        if 42 + input_dense.nb_neurons + vad_gru.nb_neurons != noise_gru.nb_inputs {
            return None;
        }
        if 42 + vad_gru.nb_neurons + noise_gru.nb_neurons != denoise_gru.nb_inputs {
            return None;
        }
        if denoise_gru.nb_neurons != denoise_output.nb_inputs {
            return None;
        }

        Some(RnnModel {
            input_dense,
            vad_gru,
            noise_gru,
            denoise_gru,
            denoise_output,
            vad_output,
        })
    }
}

impl DenseLayer {
    fn matrix(&self) -> SubMatrix<'_> {
        SubMatrix {
            data: self.input_weights.as_ref(),
            stride: self.nb_neurons,
            offset: 0,
        }
    }

    fn compute(&self, output: &mut [f32], input: &[f32]) {
        copy_i8(output, &self.bias[..]);
        self.matrix().mul_add(output, input);

        match self.activation {
            Activation::Sigmoid => {
                for out in output.iter_mut() {
                    *out = sigmoid_approx(*out * WEIGHTS_SCALE);
                }
            }
            Activation::Tanh => {
                for out in output.iter_mut() {
                    *out = tansig_approx(*out * WEIGHTS_SCALE);
                }
            }
            Activation::Relu => {
                for out in output.iter_mut() {
                    *out = relu(*out * WEIGHTS_SCALE);
                }
            }
        }
    }
}

impl GruLayer {
    fn input_submatrix(&self, offset: usize) -> SubMatrix<'_> {
        SubMatrix {
            data: self.input_weights.as_ref(),
            stride: self.nb_neurons * 3,
            offset,
        }
    }

    fn rec_submatrix(&self, offset: usize) -> SubMatrix<'_> {
        SubMatrix {
            data: self.recurrent_weights.as_ref(),
            stride: self.nb_neurons * 3,
            offset,
        }
    }

    fn compute(&self, state: &mut [f32], input: &[f32]) {
        let mut z = [0.0; MAX_NEURONS];
        let mut r = [0.0; MAX_NEURONS];
        let mut h = [0.0; MAX_NEURONS];
        let n = self.nb_neurons;

        // Compute update gate.
        copy_i8(&mut z[0..n], &self.bias[0..n]);
        self.input_submatrix(0).mul_add(&mut z[0..n], input);
        self.rec_submatrix(0).mul_add(&mut z[0..n], &state[..]);
        for z in z[0..n].iter_mut() {
            *z = sigmoid_approx(WEIGHTS_SCALE * *z);
        }

        // Compute reset gate.
        copy_i8(&mut r[0..n], &self.bias[n..(2 * n)]);
        self.input_submatrix(n).mul_add(&mut r[0..n], input);
        self.rec_submatrix(n).mul_add(&mut r[0..n], &state[..]);
        for (out, &s) in r[0..n].iter_mut().zip(&state[..]) {
            *out = s * sigmoid_approx(WEIGHTS_SCALE * *out);
        }

        // Compute output.
        copy_i8(&mut h[0..n], &self.bias[(2 * n)..]);
        self.input_submatrix(2 * n).mul_add(&mut h[0..n], input);
        self.rec_submatrix(2 * n).mul_add(&mut h[0..n], &r[0..n]);

        for (s, &z, &h) in zip3(state, &z[0..n], &h[0..n]) {
            let h = match self.activation {
                Activation::Sigmoid => sigmoid_approx(WEIGHTS_SCALE * h),
                Activation::Tanh => tansig_approx(WEIGHTS_SCALE * h),
                Activation::Relu => relu(WEIGHTS_SCALE * h),
            };
            *s = z * *s + (1.0 - z) * h;
        }
    }
}

impl RnnState {
    pub(crate) fn new(model: &RnnModel) -> RnnState {
        let vad_gru_state = vec![0.0f32; model.vad_gru.nb_neurons];
        let noise_gru_state = vec![0.0f32; model.noise_gru.nb_neurons];
        let denoise_gru_state = vec![0.0f32; model.denoise_gru.nb_neurons];
        RnnState {
            vad_gru_state,
            noise_gru_state,
            denoise_gru_state,
        }
    }

    /// Back to a silent history, allocating nothing.
    pub(crate) fn reset(&mut self) {
        self.vad_gru_state.fill(0.0);
        self.noise_gru_state.fill(0.0);
        self.denoise_gru_state.fill(0.0);
    }

    pub(crate) fn compute(
        &mut self,
        model: &RnnModel,
        gains: &mut [f32],
        vad: &mut [f32],
        input: &[f32; INPUT_SIZE],
    ) {
        let mut buf = [0.0; MAX_NEURONS * 3];
        let mut denoise_buf = [0.0; MAX_NEURONS * 3];

        let vad_gru_state = &mut self.vad_gru_state[..];
        let noise_gru_state = &mut self.noise_gru_state[..];
        let denoise_gru_state = &mut self.denoise_gru_state[..];
        model
            .input_dense
            .compute(&mut buf[0..model.input_dense.nb_neurons], input);
        model
            .vad_gru
            .compute(vad_gru_state, &buf[0..model.input_dense.nb_neurons]);
        model.vad_output.compute(vad, vad_gru_state);

        copy(&mut buf[model.input_dense.nb_neurons..], vad_gru_state);
        copy(
            &mut buf[(model.input_dense.nb_neurons + model.vad_gru.nb_neurons)..],
            input,
        );
        model.noise_gru.compute(noise_gru_state, &buf);

        copy(&mut denoise_buf, vad_gru_state);
        copy(
            &mut denoise_buf[model.vad_gru.nb_neurons..],
            noise_gru_state,
        );
        copy(
            &mut denoise_buf[(model.vad_gru.nb_neurons + model.noise_gru.nb_neurons)..],
            input,
        );
        model.denoise_gru.compute(denoise_gru_state, &denoise_buf);
        model.denoise_output.compute(gains, denoise_gru_state);
    }
}

const INPUT_SIZE: usize = 42;

fn copy(dst: &mut [f32], src: &[f32]) {
    for (x, y) in dst.iter_mut().zip(src) {
        *x = *y;
    }
}

fn copy_i8(dst: &mut [f32], src: &[i8]) {
    for (x, y) in dst.iter_mut().zip(src) {
        *x = *y as f32;
    }
}

struct SubMatrix<'a> {
    data: &'a [i8],
    stride: usize,
    offset: usize,
}

impl<'a> SubMatrix<'a> {
    fn mul_add(&self, output: &mut [f32], input: &[f32]) {
        for (col, input) in self.data.chunks_exact(self.stride).zip(input) {
            for (&x, out) in col[self.offset..].iter().zip(&mut *output) {
                *out += x as f32 * input;
            }
        }
    }
}
