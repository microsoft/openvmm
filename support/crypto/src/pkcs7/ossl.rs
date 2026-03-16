// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub(crate) struct Pkcs7Inner {
    pkcs7: openssl::pkcs7::Pkcs7,
}

pub fn pkcs7_from_der(der: &[u8]) -> Result<Pkcs7Inner, Pkcs7Error> {
    let pkcs7 = openssl::pkcs7::Pkcs7::from_der(der)
        .map_err(|e| Pkcs7Error(crate::BackendError(e, "parsing PKCS#7")))?;
    Ok(Pkcs7Inner { pkcs7 })
}

impl Pkcs7Inner {
    pub fn verify(
        &self,
        trusted_certs: &[X509Certificate],
        data: &[u8],
        flags: &X509StoreFlags,
    ) -> Result<bool, Pkcs7Error> {
        let mut store_builder = openssl::x509::store::X509StoreBuilder::new()
            .map_err(|e| Pkcs7Error(crate::BackendError(e, "building X509 store")))?;

        for cert in trusted_certs {
            store_builder
                .add_cert(cert.inner.x509.clone())
                .map_err(|e| Pkcs7Error(crate::BackendError(e, "adding certificate to store")))?;
        }

        let mut verify_flags = openssl::x509::verify::X509VerifyFlags::empty();
        if flags.partial_chain {
            verify_flags |= openssl::x509::verify::X509VerifyFlags::PARTIAL_CHAIN;
        }
        if flags.no_check_time {
            verify_flags |= openssl::x509::verify::X509VerifyFlags::NO_CHECK_TIME;
        }
        store_builder
            .set_flags(verify_flags)
            .map_err(|e| Pkcs7Error(crate::BackendError(e, "setting verify flags")))?;

        if flags.any_purpose {
            store_builder
                .set_purpose(openssl::x509::X509PurposeId::ANY)
                .map_err(|e| Pkcs7Error(crate::BackendError(e, "setting purpose")))?;
        }

        let store = store_builder.build();

        let empty_stack = openssl::stack::Stack::new()
            .map_err(|e| Pkcs7Error(crate::BackendError(e, "creating certificate stack")))?;

        match self.pkcs7.verify(
            &empty_stack,
            &store,
            Some(data),
            None,
            openssl::pkcs7::Pkcs7Flags::empty(),
        ) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}
