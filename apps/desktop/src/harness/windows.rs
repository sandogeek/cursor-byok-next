//! Loads Windows' machine Root store to answer "does the system trust this
//! CA?" without shelling out to `certutil`.

use std::{ffi::c_void, io, ptr, slice};

use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_STORE_OPEN_EXISTING_FLAG, CERT_STORE_PROV_SYSTEM_W,
    CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_LOCAL_MACHINE, CertCloseStore,
    CertEnumCertificatesInStore, CertFreeCertificateContext, CertOpenStore,
};

use super::{Error, Result};

/// `CERT_STORE_PROV_SYSTEM_W` takes the store name as a null-terminated UTF-16
/// pointer, so `ROOT` has to be widened at compile time.
const ROOT_STORE: *const u16 = windows_sys::w!("ROOT");

pub fn is_installed(cert: &str) -> Result<bool> {
    let expected = pem::parse(cert)
        .map(pem::Pem::into_contents)
        .map_err(|error| Error::Config(format!("parse CA PEM: {error}")))?;
    let store = RootStore::open()?;
    let found = store.contains(&expected);
    store.close()?;
    Ok(found)
}

/// The LocalMachine `Root` store, closed on drop when not closed explicitly.
struct RootStore(*mut c_void);

impl RootStore {
    fn open() -> Result<Self> {
        let flags = CERT_SYSTEM_STORE_LOCAL_MACHINE
            | CERT_STORE_OPEN_EXISTING_FLAG
            | CERT_STORE_READONLY_FLAG;
        // SAFETY: `CERT_STORE_PROV_SYSTEM_W` with a null-terminated wide store
        // name and a null provider handle opens the LocalMachine Root store
        // read-only.
        let handle =
            unsafe { CertOpenStore(CERT_STORE_PROV_SYSTEM_W, 0, 0, flags, ROOT_STORE.cast()) };
        if handle.is_null() {
            return Err(Error::Config(format!(
                "open the Windows LocalMachine Root store: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(Self(handle))
    }

    fn contains(&self, expected: &[u8]) -> bool {
        // SAFETY: `self.0` is an open store and the enumeration cursor starts
        // as null, which asks for the first certificate.
        let mut current =
            Certificate::owned(unsafe { CertEnumCertificatesInStore(self.0, ptr::null()) });
        while let Some(certificate) = current {
            if certificate.encoded() == expected {
                return true;
            }
            current = certificate.advance(self);
        }
        false
    }

    /// Closes the store and reports whether Windows accepted it.
    fn close(mut self) -> Result<()> {
        let handle = std::mem::replace(&mut self.0, ptr::null_mut());
        // SAFETY: `handle` came from `CertOpenStore` and is closed exactly once;
        // the null stored back tells `Drop` there is nothing left to close.
        let closed = unsafe { CertCloseStore(handle, 0) };
        if closed == 0 {
            return Err(Error::Config(format!(
                "close the Windows certificate store: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

impl Drop for RootStore {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: a non-null handle came from `CertOpenStore` and is closed once
        // here; `Drop` cannot report the failure, and `close` does when asked.
        unsafe { CertCloseStore(self.0, 0) };
    }
}

/// One `CERT_CONTEXT` owned by this module until it is advanced or dropped.
struct Certificate(*const CERT_CONTEXT);

impl Certificate {
    fn owned(context: *const CERT_CONTEXT) -> Option<Self> {
        (!context.is_null()).then_some(Self(context))
    }

    /// The DER bytes of the certificate this context points at.
    fn encoded(&self) -> &[u8] {
        // SAFETY: a context returned by the enumeration is live, and
        // `pbCertEncoded`/`cbCertEncoded` describe one encoded certificate it
        // owns.
        unsafe {
            let context = &*self.0;
            let length =
                usize::try_from(context.cbCertEncoded).expect("certificate length fits usize");
            slice::from_raw_parts(context.pbCertEncoded, length)
        }
    }

    /// Frees this context and returns the next one in the store.
    ///
    /// `CertEnumCertificatesInStore` frees the context it is handed, so the
    /// guard is disarmed rather than freed twice.
    fn advance(mut self, store: &RootStore) -> Option<Self> {
        // SAFETY: `self.0` is a live context from this store; the enumeration
        // takes ownership of it and returns the following certificate, which
        // the caller wraps again.
        let next = unsafe { CertEnumCertificatesInStore(store.0, self.0) };
        self.0 = ptr::null();
        Self::owned(next)
    }
}

impl Drop for Certificate {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: the context is still owned by this guard and is freed once.
        unsafe { CertFreeCertificateContext(self.0) };
    }
}
