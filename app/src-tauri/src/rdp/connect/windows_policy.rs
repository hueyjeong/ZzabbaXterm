use std::{mem::size_of, ptr};

use schannel::{
    RawPointer,
    cert_context::CertContext,
    cert_store::{CertStore, CertStoreBuilder, Memory},
};
use tokio_rustls::rustls::pki_types::CertificateDer;
use windows_sys::Win32::{
    Foundation::{CRYPT_E_NOT_FOUND, GetLastError},
    Security::Cryptography::*,
};

use super::Error;

struct Chain(*mut CERT_CHAIN_CONTEXT);

impl Drop for Chain {
    fn drop(&mut self) {
        // SAFETY: Only a successful CertGetCertificateChain result is wrapped.
        unsafe { CertFreeCertificateChain(self.0) };
    }
}

fn reject_disallowed(cert: &CertContext, stores: [&CertStore; 2]) -> Result<(), Error> {
    // SAFETY: The borrowed handles stay live, and any matching context is released once.
    unsafe {
        for store in stores {
            let found = CertFindCertificateInStore(
                store.as_ptr(),
                X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                0,
                CERT_FIND_EXISTING,
                cert.as_ptr().cast_const(),
                ptr::null(),
            );
            if !found.is_null() {
                CertFreeCertificateContext(found);
                return Err(Error::TlsIdentity);
            }
            if GetLastError() != CRYPT_E_NOT_FOUND as u32 {
                return Err(Error::TlsIdentity);
            }
        }
    }
    Ok(())
}

/// 고정(pin)으로 체인 검증을 대체한 경우에도 Windows Disallowed 저장소는 확인한다.
pub(super) fn verify_disallowed(peer: &[CertificateDer<'_>]) -> Result<(), Error> {
    let mut builder = CertStoreBuilder::new();
    builder.read_only();
    let user = builder.open_current_user("Disallowed").map_err(|_| Error::TlsIdentity)?;
    let machine = builder.open_local_machine("Disallowed").map_err(|_| Error::TlsIdentity)?;
    let disallowed = [&user, &machine];
    let mut presented = Memory::new().map_err(|_| Error::TlsIdentity)?;
    for cert in peer {
        let cert = presented.add_encoded_certificate(cert.as_ref()).map_err(|_| Error::TlsIdentity)?;
        reject_disallowed(&cert, disallowed)?;
    }
    Ok(())
}

pub(super) fn verify(name: &str, peer: &[CertificateDer<'_>]) -> Result<(), Error> {
    let (leaf, intermediates) = peer.split_first().ok_or(Error::TlsIdentity)?;
    let mut builder = CertStoreBuilder::new();
    builder.read_only();
    let mut presented = Memory::new().map_err(|_| Error::TlsIdentity)?;
    let leaf = presented.add_encoded_certificate(leaf.as_ref()).map_err(|_| Error::TlsIdentity)?;
    for cert in intermediates {
        presented.add_encoded_certificate(cert.as_ref()).map_err(|_| Error::TlsIdentity)?;
    }
    verify_disallowed(peer)?;
    let presented = presented.into_store();
    let mut server_auth = szOID_PKIX_KP_SERVER_AUTH.cast_mut();
    let params = CERT_CHAIN_PARA {
        cbSize: size_of::<CERT_CHAIN_PARA>() as u32,
        RequestedUsage: CERT_USAGE_MATCH {
            dwType: USAGE_MATCH_TYPE_AND,
            Usage: CTL_USAGE { cUsageIdentifier: 1, rgpszUsageIdentifier: &mut server_auth },
        },
        ..Default::default()
    };
    let mut wide_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut ssl = HTTPSPolicyCallbackData {
        Anonymous: HTTPSPolicyCallbackData_0 { cbSize: size_of::<HTTPSPolicyCallbackData>() as u32 },
        dwAuthType: AUTHTYPE_SERVER,
        fdwChecks: 0,
        pwszServerName: wide_name.as_mut_ptr(),
    };
    let policy = CERT_CHAIN_POLICY_PARA {
        cbSize: size_of::<CERT_CHAIN_POLICY_PARA>() as u32,
        pvExtraPolicyPara: ptr::from_mut(&mut ssl).cast(),
        ..Default::default()
    };
    let mut status = CERT_CHAIN_POLICY_STATUS {
        cbSize: size_of::<CERT_CHAIN_POLICY_STATUS>() as u32,
        ..Default::default()
    };

    // SAFETY: Schannel owns the store/cert handles; the API inputs and output buffers
    // stay live during each call. Returned contexts are released once.
    unsafe {
        let mut chain = ptr::null_mut();
        // The current-user engine supplies OS trust and Disallowed CTLs. The peer store
        // supplies issuers only; it never supplies new trust anchors.
        if CertGetCertificateChain(
            ptr::null_mut(), leaf.as_ptr().cast(), ptr::null(), presented.as_ptr(),
            &params, CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL | CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE,
            ptr::null(), &mut chain,
        ) == 0 || chain.is_null() {
            return Err(Error::TlsIdentity);
        }
        let chain = Chain(chain);
        if (*chain.0).TrustStatus.dwErrorStatus & CERT_TRUST_IS_EXPLICIT_DISTRUST != 0
            || CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL, chain.0, &policy, &mut status) == 0
            || status.dwError != 0
        {
            return Err(Error::TlsIdentity);
        }
    }
    Ok(())
}
