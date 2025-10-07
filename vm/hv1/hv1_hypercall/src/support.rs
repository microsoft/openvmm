// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for parsing and handling hypercalls.

use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use hvdef::HV_PAGE_SIZE;
use hvdef::HV_PAGE_SIZE_USIZE;
use hvdef::HvError;
use hvdef::HvResult;
use hvdef::HypercallCode;
use hvdef::hypercall::Control;
use hvdef::hypercall::HypercallOutput;
use std::marker::PhantomData;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Ref;

/// A hypercall definition.
#[derive(Copy, Clone)]
pub enum HypercallData {
    /// A non-rep hypercall.
    Simple {
        /// The fixed input size.
        input_size: usize,
        /// The fixed output size.
        output_size: usize,
        /// If true, the input is variable sized.
        is_variable: bool,
    },
    /// A rep hypercall.
    Rep {
        /// The fixed input size.
        header_size: usize,
        /// The input element size.
        input_element_size: usize,
        /// The output element size.
        output_element_size: usize,
        /// If true, the input is variable sized.
        is_variable: bool,
    },
    /// A VTL switch hypercall.
    Vtl,
}

/// Parameters to pass to a hypercall dispatch function.
pub struct HypercallParameters<'a> {
    control: Control,
    input: &'a [u8],
    output: &'a mut [u8],
}

/// `[u64; 2]` buffer aligned to 16 bytes for hypercall inputs.
#[repr(C, align(16))]
#[derive(Copy, Clone)]
struct HypercallAlignedBuf128<const N: usize>([[u64; 2]; N]);

impl<const N: usize> HypercallAlignedBuf128<N> {
    fn new_zeroed() -> Self {
        Self([[0, 0]; N])
    }
}

type HypercallAlignedPage = HypercallAlignedBuf128<{ HV_PAGE_SIZE_USIZE / 16 }>;

/// A helper for dispatching hypercalls.
struct InnerDispatcher<'a, T> {
    control: Control,
    guest_memory: &'a GuestMemory,
    handler: T,
}

#[derive(Debug, Error)]
enum HypercallParseError {
    #[error("invalid control: {0:?}")]
    InvalidControl(Control),
    #[error("hypercall input too large for fast hypercall")]
    TooBigForFast,
    #[error("input/output straddles a page boundary")]
    Straddle,
    #[error("memory access error")]
    Access(#[source] GuestMemoryError),
    #[error("unaligned memory access")]
    Unaligned,
}

impl From<HypercallParseError> for HvError {
    fn from(err: HypercallParseError) -> Self {
        tracing::warn!(
            error = &err as &dyn std::error::Error,
            "hypercall parse failure"
        );
        match err {
            HypercallParseError::Unaligned => Self::InvalidAlignment,
            _ => Self::InvalidHypercallInput,
        }
    }
}

/// Trait for getting the handler from the `handler` parameter passed to
/// [`Dispatcher::dispatch`].
///
/// This is useful when the handler parameter is a wrapper that applies a
/// specific hypercall calling convention.
pub trait AsHandler<H> {
    /// Gets the inner handler.
    fn as_handler(&mut self) -> &mut H;
}

impl<'a, T: HypercallIo> InnerDispatcher<'a, T> {
    /// Creates a new dispatcher.
    fn new(guest_memory: &'a GuestMemory, mut handler: T) -> Self {
        Self {
            control: handler.control().into(),
            guest_memory,
            handler,
        }
    }

    /// Gets the hypercall code being dispatched.
    fn code(&self) -> HypercallCode {
        HypercallCode(self.control.code())
    }

    /// Logs an unsupported hypercall and returns the appropriate error.
    fn unhandled(&self) -> Option<HypercallOutput> {
        tracelimit::warn_ratelimited!(code = ?self.code(), "no handler for hypercall code");
        Some(HvError::InvalidHypercallCode.into())
    }

    /// Complete hypercall handling.
    fn complete(&mut self, output: Option<HypercallOutput>) {
        if let Some(output) = output {
            if output.call_status() == Err(HvError::Timeout).into() {
                self.handler.retry(
                    self.control
                        .with_rep_start(output.elements_processed())
                        .into(),
                );
            } else {
                self.handler.set_result(output.into());
                self.handler.advance_ip();
            }
        }
    }

    fn dispatch_dyn<H>(
        &mut self,
        data: &HypercallData,
        dispatch: fn(&mut H, HypercallParameters<'_>) -> HypercallOutput,
    ) -> Option<HypercallOutput>
    where
        T: AsHandler<H>,
    {
        self.dispatch_inner(data, dispatch)
            .unwrap_or_else(|err| Some(err.into()))
    }

    fn dispatch_inner<H>(
        &mut self,
        data: &HypercallData,
        dispatch: fn(&mut H, HypercallParameters<'_>) -> HypercallOutput,
    ) -> Result<Option<HypercallOutput>, HvError>
    where
        T: AsHandler<H>,
    {
        tracing::trace!(code = ?self.code(), "hypercall");
        let control = self.control;

        let (input_len, output_start, output_len, out_elem_size) = match *data {
            HypercallData::Vtl => {
                let input = self.handler.vtl_input();
                let _ = (dispatch)(
                    self.handler.as_handler(),
                    HypercallParameters {
                        control,
                        input: input.as_bytes(),
                        output: &mut [],
                    },
                );
                return Ok(None);
            }
            HypercallData::Simple {
                input_size,
                output_size,
                is_variable,
            } => {
                if control.rep_count() != 0
                    || control.rep_start() != 0
                    || (!is_variable && control.variable_header_size() != 0)
                {
                    return Err(HypercallParseError::InvalidControl(control).into());
                }

                let input_size = input_size + control.variable_header_size() * 8;
                (input_size, 0, output_size, 0)
            }
            HypercallData::Rep {
                header_size,
                input_element_size,
                output_element_size,
                is_variable,
            } => {
                if control.rep_count() == 0
                    || (!is_variable && control.variable_header_size() != 0)
                    || control.rep_start() >= control.rep_count()
                {
                    return Err(HypercallParseError::InvalidControl(control).into());
                }

                let input_len = header_size
                    + control.variable_header_size() * 8
                    + input_element_size * control.rep_count();
                let output_start = output_element_size * control.rep_start();
                let output_len = output_element_size * control.rep_count();
                (input_len, output_start, output_len, output_element_size)
            }
        };

        let mut input_buffer = HypercallAlignedPage::new_zeroed();
        let mut output_buffer = HypercallAlignedPage::new_zeroed();

        let ret = if control.fast() {
            let input_regpairs = input_len.div_ceil(16);
            let output_regpairs = output_len.div_ceil(16);
            if self.handler.fast_register_pair_count() < input_regpairs
                || self.handler.fast_register_pair_count() - input_regpairs < output_regpairs
                || (output_regpairs > 0 && !self.handler.extended_fast_hypercalls_ok())
            {
                return Err(HypercallParseError::TooBigForFast.into());
            }

            let input = &mut input_buffer.0[..input_regpairs];
            let output = &mut output_buffer.0[..output_regpairs];

            // Read in the input.
            let output_start_index = self.handler.fast_input(input, output_regpairs);
            let completed_output_pairs = output_start / 16;
            let (new_output_index, completed_output_pairs) = match output_start % 16 {
                0 => (
                    output_start_index + completed_output_pairs,
                    completed_output_pairs,
                ),
                _ => {
                    // There are some number of completed output pairs, and one partial pair.
                    // Copy the partial register pair from the previous output to the appropriate
                    // location in the output buffer.
                    let partial_output_index = output_start_index + completed_output_pairs;
                    self.handler.fast_regs(
                        partial_output_index,
                        &mut output[completed_output_pairs..completed_output_pairs + 1],
                    );
                    (partial_output_index, completed_output_pairs)
                }
            };

            let ret = (dispatch)(
                self.handler.as_handler(),
                HypercallParameters {
                    control,
                    input: &input.as_bytes()[..input_len],
                    output: &mut output.as_mut_bytes()[..output_len],
                },
            );

            // For rep hypercalls, always write back the completed number of reps (which may be 0).
            // For simple hypercalls, on success write back all output. On failure (and timeout,
            // which is handled as a failure), nothing is written back.
            let output_end = if out_elem_size > 0 {
                out_elem_size * ret.elements_processed()
            } else if ret.call_status().is_ok() {
                output_len
            } else {
                0
            };

            let output_regpairs = output_end.div_ceil(16);

            // Only need to write back output regpairs that were not previously completely written
            // out, at the new output location.
            let output = &output[completed_output_pairs..output_regpairs];
            self.handler.fast_output(new_output_index, output);
            ret
        } else {
            let check_buffer = |gpa: u64, len: usize| {
                // All IO must fit within a single page.
                if (len as u64) > (HV_PAGE_SIZE - gpa % HV_PAGE_SIZE) {
                    return Err(HvError::from(HypercallParseError::Straddle));
                }

                // The buffer must be 8 byte aligned.
                if len != 0 && !gpa.is_multiple_of(8) {
                    return Err(HvError::from(HypercallParseError::Unaligned));
                }

                Ok(())
            };

            check_buffer(self.handler.input_gpa(), input_len)?;
            check_buffer(self.handler.output_gpa(), output_len)?;

            let input = &mut input_buffer.0.as_mut_bytes()[..input_len];
            let output = &mut output_buffer.0.as_mut_bytes()[..output_len];

            // FUTURE: consider copying only the header and entries after
            // `rep_start` for rep hypercalls.
            self.guest_memory
                .read_at(self.handler.input_gpa(), input)
                .map_err(HypercallParseError::Access)?;

            let output_gpa = self.handler.output_gpa();

            let ret = (dispatch)(
                self.handler.as_handler(),
                HypercallParameters {
                    control,
                    input,
                    output,
                },
            );

            // For rep hypercalls, always write back the completed number of reps (which may be 0).
            // For simple hypercalls, on success write back all output. On failure (and timeout,
            // which is handled as a failure), nothing is written back.
            let output_end = if out_elem_size > 0 {
                out_elem_size * ret.elements_processed()
            } else if ret.call_status().is_ok() {
                output_len
            } else {
                0
            };

            self.guest_memory
                .write_at(
                    output_gpa.wrapping_add(output_start as u64),
                    &output[output_start..output_end],
                )
                .map_err(HypercallParseError::Access)?;

            ret
        };

        if ret.call_status().is_ok() {
            debug_assert_eq!(ret.elements_processed(), control.rep_count());
        }

        Ok(Some(ret))
    }
}

/// Provides input and output parameters for a hypercall.
pub trait HypercallIo {
    /// Advances the instruction pointer for a completed hypercall.
    ///
    /// Either `advance_ip` or `retry` will be called.
    fn advance_ip(&mut self);

    /// Retains the instruction pointer at the hypercall point so that the
    /// hypercall will be retried.
    ///
    /// Either `advance_ip` or `retry` will be called.
    /// `control` is the updated hypercall input value to use in the retry.
    fn retry(&mut self, control: u64);

    /// The hypercall input value.
    fn control(&mut self) -> u64;

    /// The guest address of the hypercall input.
    fn input_gpa(&mut self) -> u64;

    /// The guest address of the hypercall output.
    fn output_gpa(&mut self) -> u64;

    /// Returns the maximum number of fast register pairs.
    fn fast_register_pair_count(&mut self) -> usize;

    /// Returns whether extended fast hypercall input/output is allowed.
    fn extended_fast_hypercalls_ok(&mut self) -> bool;

    /// Fills the buffer with fast input parameters. Given an output size in
    /// register pairs, returns the index of the first output register pair.
    fn fast_input(&mut self, buf: &mut [[u64; 2]], output_register_pairs: usize) -> usize;

    /// Writes fast output registers from the buffer.
    fn fast_output(&mut self, starting_pair_index: usize, buf: &[[u64; 2]]);

    /// The VTL switch hypercall input parameter.
    fn vtl_input(&mut self) -> u64;

    /// Sets the hypercall result.
    fn set_result(&mut self, n: u64);

    /// Reads fast input/output registers into a buffer, given the starting pair index.
    fn fast_regs(&mut self, starting_pair_index: usize, buf: &mut [[u64; 2]]);
}

impl<T: HypercallIo> HypercallIo for &mut T {
    fn advance_ip(&mut self) {
        (**self).advance_ip()
    }

    fn retry(&mut self, control: u64) {
        (**self).retry(control)
    }

    fn control(&mut self) -> u64 {
        (**self).control()
    }

    fn input_gpa(&mut self) -> u64 {
        (**self).input_gpa()
    }

    fn output_gpa(&mut self) -> u64 {
        (**self).output_gpa()
    }

    fn fast_register_pair_count(&mut self) -> usize {
        (**self).fast_register_pair_count()
    }

    fn extended_fast_hypercalls_ok(&mut self) -> bool {
        (**self).extended_fast_hypercalls_ok()
    }

    fn fast_input(&mut self, buf: &mut [[u64; 2]], output_register_pairs: usize) -> usize {
        (**self).fast_input(buf, output_register_pairs)
    }

    fn fast_output(&mut self, starting_pair_index: usize, buf: &[[u64; 2]]) {
        (**self).fast_output(starting_pair_index, buf)
    }

    fn vtl_input(&mut self) -> u64 {
        (**self).vtl_input()
    }

    fn set_result(&mut self, n: u64) {
        (**self).set_result(n)
    }

    fn fast_regs(&mut self, starting_pair_index: usize, buf: &mut [[u64; 2]]) {
        (**self).fast_regs(starting_pair_index, buf)
    }
}

/// A trait defined on dummy objects to provide metadata for a hypercall.
pub trait HypercallDefinition {
    /// The hypercall code.
    const CODE: HypercallCode;
    /// The associated hypercall metadata.
    const DATA: HypercallData;
}

/// A trait to dispatch an individual hypercall.
pub trait HypercallDispatch<T> {
    /// Dispatch this hypercall.
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput;
}

/// A simple, non-variable hypercall.
pub struct SimpleHypercall<In, Out, const CODE: u16>(PhantomData<(In, Out)>);

impl<In, Out, const CODE: u16> SimpleHypercall<In, Out, CODE>
where
    In: IntoBytes + FromBytes + Immutable + KnownLayout,
    Out: IntoBytes + FromBytes + Immutable + KnownLayout,
{
    /// Parses the hypercall parameters to input and output types.
    pub fn parse(params: HypercallParameters<'_>) -> (&In, &mut Out) {
        (
            FromBytes::ref_from_prefix(params.input).unwrap().0, // TODO: zerocopy: ref-from-prefix: use-rest-of-range, err (https://github.com/microsoft/openvmm/issues/759)
            FromBytes::mut_from_prefix(params.output).unwrap().0, // TODO: zerocopy: mut-from-prefix: use-rest-of-range, err (https://github.com/microsoft/openvmm/issues/759)
        )
    }

    pub fn run(
        params: HypercallParameters<'_>,
        f: impl FnOnce(&In) -> HvResult<Out>,
    ) -> HypercallOutput {
        let (input, output) = Self::parse(params);
        match f(input) {
            Ok(r) => {
                *output = r;
                HypercallOutput::SUCCESS
            }
            Err(e) => HypercallOutput::from(e),
        }
    }
}

impl<In, Out, const CODE: u16> HypercallDefinition for SimpleHypercall<In, Out, CODE> {
    const CODE: HypercallCode = HypercallCode(CODE);

    const DATA: HypercallData = HypercallData::Simple {
        input_size: size_of::<In>(),
        output_size: size_of::<Out>(),
        is_variable: false,
    };
}

/// A simple variable hypercall.
pub struct VariableHypercall<In, Out, const CODE: u16>(PhantomData<(In, Out)>);

impl<In, Out, const CODE: u16> VariableHypercall<In, Out, CODE>
where
    In: IntoBytes + FromBytes + Immutable + KnownLayout,
    Out: IntoBytes + FromBytes + Immutable + KnownLayout,
{
    /// Parses the hypercall parameters to input and output types.
    pub fn parse(params: HypercallParameters<'_>) -> (&In, &[u64], &mut Out) {
        let (input, rest) = Ref::<_, In>::from_prefix(params.input).unwrap();
        (
            Ref::into_ref(input),
            <[u64]>::ref_from_bytes(rest).unwrap(), //TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
            Out::mut_from_prefix(params.output).unwrap().0, //TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
        )
    }

    pub fn run(
        params: HypercallParameters<'_>,
        f: impl FnOnce(&In, &[u64]) -> HvResult<Out>,
    ) -> HypercallOutput {
        let (input, var_header, output) = Self::parse(params);
        match f(input, var_header) {
            Ok(r) => {
                *output = r;
                HypercallOutput::SUCCESS
            }
            Err(e) => HypercallOutput::from(e),
        }
    }
}

impl<In, Out, const CODE: u16> HypercallDefinition for VariableHypercall<In, Out, CODE> {
    const CODE: HypercallCode = HypercallCode(CODE);

    const DATA: HypercallData = HypercallData::Simple {
        input_size: size_of::<In>(),
        output_size: size_of::<Out>(),
        is_variable: true,
    };
}

/// A rep hypercall.
pub struct RepHypercall<Hdr, In, Out, const CODE: u16>(PhantomData<(Hdr, In, Out)>);

/// Hypervisor result type for rep hypercalls. These hypercalls have either no or only rep output
/// data, which is passed separately from the result. The error is an a tuple consisting of an
/// `HvError` and the number of elements successfully processed prior to the error being returned.
/// An `Ok` result implies that all input elements were processed successfully.
pub type HvRepResult = Result<(), (HvError, usize)>;

impl<Hdr, In, Out, const CODE: u16> RepHypercall<Hdr, In, Out, CODE>
where
    Hdr: IntoBytes + FromBytes + Immutable + KnownLayout,
    In: IntoBytes + FromBytes + Immutable + KnownLayout,
    Out: IntoBytes + FromBytes + Immutable + KnownLayout,
{
    /// Parses the hypercall parameters to input and output types.
    pub fn parse(params: HypercallParameters<'_>) -> (&Hdr, &[In], &mut [Out]) {
        let (header, rest) = Ref::<_, Hdr>::from_prefix(params.input).unwrap();
        let input = if size_of::<In>() == 0 {
            &[]
        } else {
            // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
            &<[In]>::ref_from_bytes(rest).unwrap()[params.control.rep_start()..]
        };
        let output = if size_of::<Out>() == 0 {
            &mut []
        } else {
            // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
            &mut <[Out]>::mut_from_prefix_with_elems(
                params.output,
                params.output.len() / size_of::<Out>(),
            )
            .unwrap()
            .0[params.control.rep_start()..]
        };

        (Ref::into_ref(header), input, output)
    }

    pub fn run(
        params: HypercallParameters<'_>,
        f: impl FnOnce(&Hdr, &[In], &mut [Out]) -> HvRepResult,
    ) -> HypercallOutput {
        let control = params.control;
        let (header, input, output) = Self::parse(params);
        match f(header, input, output) {
            Ok(()) => HypercallOutput::SUCCESS.with_elements_processed(control.rep_count()),
            Err((e, reps)) => {
                assert!(
                    control.rep_start() + reps < control.rep_count(),
                    "more reps processed than requested"
                );
                HypercallOutput::from(e).with_elements_processed(control.rep_start() + reps)
            }
        }
    }
}

impl<Hdr, In, Out, const CODE: u16> HypercallDefinition for RepHypercall<Hdr, In, Out, CODE> {
    const CODE: HypercallCode = HypercallCode(CODE);

    const DATA: HypercallData = HypercallData::Rep {
        header_size: size_of::<Hdr>(),
        input_element_size: size_of::<In>(),
        output_element_size: size_of::<Out>(),
        is_variable: false,
    };
}

/// A variable rep hypercall.
pub struct VariableRepHypercall<Hdr, In, Out, const CODE: u16>(PhantomData<(Hdr, In, Out)>);

impl<Hdr, In, Out, const CODE: u16> VariableRepHypercall<Hdr, In, Out, CODE>
where
    Hdr: IntoBytes + FromBytes + Immutable + KnownLayout,
    In: IntoBytes + FromBytes + Immutable + KnownLayout,
    Out: IntoBytes + FromBytes + Immutable + KnownLayout,
{
    /// Parses the hypercall parameters to input and output types.
    pub fn parse(params: HypercallParameters<'_>) -> (&Hdr, &[u64], &[In], &mut [Out]) {
        let (header, rest) = Ref::<_, Hdr>::from_prefix(params.input).unwrap();
        let (var_header, rest) =
            <[u64]>::ref_from_prefix_with_elems(rest, params.control.variable_header_size())
                .unwrap();
        let input = if size_of::<In>() == 0 {
            &[]
        } else {
            &<[In]>::ref_from_bytes(rest).unwrap()[params.control.rep_start()..]
        };
        let output = if size_of::<Out>() == 0 {
            &mut []
        } else {
            // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
            &mut <[Out]>::mut_from_prefix_with_elems(
                params.output,
                params.output.len() / size_of::<Out>(),
            )
            .unwrap()
            .0[params.control.rep_start()..]
        };
        (Ref::into_ref(header), var_header, input, output)
    }

    pub fn run(
        params: HypercallParameters<'_>,
        f: impl FnOnce(&Hdr, &[u64], &[In], &mut [Out]) -> HvRepResult,
    ) -> HypercallOutput {
        let control = params.control;
        let (header, var_header, input, output) = Self::parse(params);
        match f(header, var_header, input, output) {
            Ok(()) => HypercallOutput::SUCCESS.with_elements_processed(control.rep_count()),
            Err((e, reps)) => {
                assert!(
                    control.rep_start() + reps < control.rep_count(),
                    "more reps processed than requested"
                );
                HypercallOutput::from(e).with_elements_processed(control.rep_start() + reps)
            }
        }
    }
}

impl<Hdr, In, Out, const CODE: u16> HypercallDefinition
    for VariableRepHypercall<Hdr, In, Out, CODE>
{
    const CODE: HypercallCode = HypercallCode(CODE);

    const DATA: HypercallData = HypercallData::Rep {
        header_size: size_of::<Hdr>(),
        input_element_size: size_of::<In>(),
        output_element_size: size_of::<Out>(),
        is_variable: true,
    };
}

/// A VTL switch hypercall.
pub struct VtlHypercall<const CODE: u16>(());

impl<const CODE: u16> VtlHypercall<CODE> {
    pub fn parse(params: HypercallParameters<'_>) -> (u64, Control) {
        (u64::read_from_bytes(params.input).unwrap(), params.control)
    }

    pub fn run(params: HypercallParameters<'_>, f: impl FnOnce(u64, Control)) -> HypercallOutput {
        let (input, control) = Self::parse(params);
        f(input, control);
        HypercallOutput::SUCCESS
    }
}

impl<const CODE: u16> HypercallDefinition for VtlHypercall<CODE> {
    const CODE: HypercallCode = HypercallCode(CODE);
    const DATA: HypercallData = HypercallData::Vtl;
}

/// Creates a hypercall dispatcher, where the dispatcher can support any of the
/// list of provided hypercalls.
///
/// ```ignore
/// hv1_hypercall::dispatcher!(
///     Self,
///     &guest_memory,
///     [
///         hv1_hypercall::HvPostMessage,
///         hv1_hypercall::HvSignalEvent,
///         #[cfg(guest_arch = "x86_64")]
///         hv1_hypercall::HvX64StartVirtualProcessor,
///     ],
/// );
/// ```
#[macro_export]
macro_rules! dispatcher {
    ($handler:ty, [ $($(#[$a:meta])* $hc:ty),* $(,)? ] $(,)?) => {
        {
            use $crate::{Dispatcher, HypercallDefinition, HypercallHandler};

            Dispatcher::<$handler>::new(|hc| match hc {
                $(
                $(#[$a])*
                <$hc as HypercallDefinition>::CODE => Some(HypercallHandler::new::<$hc>()),
                )*
                _ => None,
            })
        }
    };
}

/// Hypercall dispatcher.
///
/// Construct with [`dispatcher!`].
pub struct Dispatcher<H> {
    lookup: fn(HypercallCode) -> Option<HypercallHandler<H>>,
}

#[doc(hidden)]
pub struct HypercallHandler<H> {
    data: &'static HypercallData,
    f: fn(&mut H, HypercallParameters<'_>) -> HypercallOutput,
}

impl<H> HypercallHandler<H> {
    pub fn new<C: HypercallDefinition>() -> Self
    where
        H: HypercallDispatch<C>,
    {
        Self {
            data: &C::DATA,
            f: H::dispatch,
        }
    }
}

impl<H> Dispatcher<H> {
    #[doc(hidden)]
    pub const fn new(lookup: fn(HypercallCode) -> Option<HypercallHandler<H>>) -> Self {
        Self { lookup }
    }

    /// Dispatches a hypercall.
    pub fn dispatch(&self, guest_memory: &GuestMemory, handler: impl HypercallIo + AsHandler<H>) {
        let mut dispatcher = InnerDispatcher::new(guest_memory, handler);
        let result = match (self.lookup)(dispatcher.code()) {
            Some(x) => dispatcher.dispatch_dyn(x.data, x.f),
            None => dispatcher.unhandled(),
        };
        dispatcher.complete(result);
    }
}
