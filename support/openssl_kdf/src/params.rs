// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// See also the LICENSE file in the root of the crate for additional copyright
// information.

use crate::cvt_p;
use crate::sys::OSSL_PARAM_construct_int;
use crate::sys::OSSL_PARAM_construct_octet_string;
use crate::sys::OSSL_PARAM_construct_utf8_string;
use crate::sys::OSSL_PARAM;
use crate::sys::OSSL_PARAM_END;
use libc::c_char;
use libc::c_int;
use libc::c_void;
use openssl::error::ErrorStack;
use openssl_sys as ffi;
use std::ffi::CStr;
use std::fmt;
use std::ptr;

enum Param {
    I32(*mut c_int),
    String(*mut c_char, usize),
    Vec(*mut c_void, usize),
}

impl Param {
    fn alloc_i32(val: i32) -> Result<Param, ErrorStack> {
        // SAFETY: Passing a non-zero size and immediately validating the return value.
        let p = (unsafe {
            cvt_p(ffi::CRYPTO_malloc(
                size_of::<c_int>(),
                concat!(file!(), "\0").as_ptr().cast(),
                line!() as c_int,
            ))
        }?)
        .cast::<c_int>();
        // SAFETY: We have validated that p is non-null.
        unsafe { *p = val };

        Ok(Param::I32(p))
    }

    fn alloc_string(val: &CStr) -> Result<Param, ErrorStack> {
        Ok(Param::String(
            (alloc_slice_inner(val.to_bytes_with_nul())?).cast(),
            val.to_bytes_with_nul().len(),
        ))
    }

    fn alloc_vec(val: &[u8]) -> Result<Param, ErrorStack> {
        Ok(Param::Vec(alloc_slice_inner(val)?, val.len()))
    }
}

fn alloc_slice_inner(val: &[u8]) -> Result<*mut c_void, ErrorStack> {
    Ok(if val.is_empty() {
        ptr::null_mut()
    } else {
        // SAFETY: Passing a non-zero size and immediately validating the return value.
        let p = unsafe {
            cvt_p(ffi::CRYPTO_malloc(
                val.len(),
                concat!(file!(), "\0").as_ptr().cast(),
                line!() as c_int,
            ))
        }?;
        // SAFETY: We have validated that p is non-null, and it will be the same length as val.
        unsafe { ptr::copy_nonoverlapping(val.as_ptr(), p.cast::<u8>(), val.len()) };
        p
    })
}

macro_rules! drop_param {
    ($p:ident) => {{
        ffi::CRYPTO_free(
            $p.cast::<c_void>(),
            concat!(file!(), "\0").as_ptr().cast(),
            line!() as c_int,
        );
    }};
}

impl Drop for Param {
    fn drop(&mut self) {
        // SAFETY: Params are guaranteed to be allocated via CRYPTO_malloc and valid.
        unsafe {
            match *self {
                Param::I32(p) => drop_param!(p),
                Param::String(p, _) => drop_param!(p),
                Param::Vec(p, _) => drop_param!(p),
            }
        }
    }
}

pub struct ParamsBuilder(Vec<(&'static CStr, Param)>);

impl ParamsBuilder {
    pub fn with_capacity(capacity: usize) -> Self {
        let params = Vec::with_capacity(capacity);
        Self(params)
    }

    pub fn build(self) -> Params {
        let len = self.0.len();

        let mut params = Params {
            fixed: self.0,
            output: Vec::with_capacity(len + 1),
        };

        // Mapping each argument held in the builder, and mapping them to a new output Vec.
        // This new output vec is to be consumed by a EVP_KDF_CTX_set_params or similar function
        // the output vec references data held in the first vec.
        // Data is allocated by the openssl allocator, so assumed in a memory stable realm.
        // It's important the data does not move from the time we create the "output" slice and the
        // moment it's read by the EVP_KDF_CTX_set_params functions.
        for (name, p) in &mut params.fixed {
            use Param::*;
            // SAFETY: Name is guaranteed to be a valid C string, and the bufs are only constructed by alloc_slice_inner,
            // which makes sure they are valid and have correct lengths.
            let v = unsafe {
                match p {
                    I32(v) => OSSL_PARAM_construct_int(name.as_ptr(), *v),
                    Vec(buf, len) => OSSL_PARAM_construct_octet_string(name.as_ptr(), *buf, *len),
                    String(buf, len) => OSSL_PARAM_construct_utf8_string(name.as_ptr(), *buf, *len),
                }
            };
            params.output.push(v);
        }
        params.output.push(OSSL_PARAM_END);
        params
    }
}

macro_rules! add_construct {
    ($func:ident, $name:ident, $ty:ty) => {
        impl ParamsBuilder {
            pub fn $func(&mut self, key: &'static CStr, val: $ty) -> Result<(), ErrorStack> {
                self.0.push((key, Param::$name(val)?));
                Ok(())
            }
        }
    };
}

add_construct!(add_i32, alloc_i32, i32);
add_construct!(add_string, alloc_string, &CStr);
add_construct!(add_slice, alloc_vec, &[u8]);
// TODO(baloo): add u32, etc

pub struct Params {
    fixed: Vec<(&'static CStr, Param)>,
    output: Vec<OSSL_PARAM>,
}

impl Params {
    pub fn len(&self) -> usize {
        self.output.len()
    }

    pub fn as_mut_ptr(&mut self) -> *mut OSSL_PARAM {
        self.output.as_mut_ptr()
    }

    pub fn as_ptr(&mut self) -> *const OSSL_PARAM {
        self.output.as_ptr()
    }
}

impl fmt::Debug for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Params([")?;
        for o in &self.output {
            write!(f, "OSSL_PARAM {{")?;
            if o.data_type != 0 {
                // SAFETY: key is guaranteed by construction to be a valid c string.
                write!(f, "name = {:?}, ", unsafe { CStr::from_ptr(o.key) })?;
                write!(f, "buf = {:?}, ", o.data)?;
                write!(f, "len = {:?}", o.data_size)?;
            } else {
                write!(f, "END")?;
            }

            write!(f, "}}, ")?;
        }
        write!(f, "])")
    }
}
