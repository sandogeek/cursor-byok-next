//! Loads Windows' machine Root store to answer "does the system trust this
//! CA?" without shelling out to `certutil`.

use std::{ffi::c_void, io, ptr, slice};

use windows_sys::Win32::Security::Cryptography::{
    CERT_STORE_OPEN_EXISTING_FLAG, CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG,
    CERT_SYSTEM_STORE_LOCAL_MACHINE, CertCloseStore, CertEnumCertificatesInStore, CertOpenStore,
};

use super::{Error, Result};

/// `CERT_STORE_PROV_SYSTEM_W` takes the store name as a null-terminated UTF-16
/// pointer, so `ROOT` has to be widened at compile time.
const ROOT_STORE: *const u16 = windows_sys::w!("ROOT");

pub fn is_installed(cert: &str) -> Result<bool> {
    let expected = pem::parse(cert)
        .map(pem::Pem::into_contents)
        .map_err(|error| Error::Config(format!("parse CA PEM: {error}")))?;
    let store = open_root_store()?;
    let mut found = false;
    // SAFETY: `store` was returned by `open_root_store` and is still open; the
    // enumeration cursor starts null and each returned context stays owned by
    // the enumeration until the next call.
    let mut cursor = unsafe { CertEnumCertificatesInStore(store, ptr::null()) };
    while !cursor.is_null() {
        // SAFETY: a non-null cursor returned by `CertEnumCertificatesInStore`
        // points at a live `CERT_CONTEXT` whose fields are initialized.
        let pb_cert_encoded = unsafe { (*cursor).pbCertEncoded };
        // SAFETY: same live `CERT_CONTEXT`; `cbCertEncoded` is its sibling field
        // and is initialized by the same enumeration.
        let cb_cert_encoded = unsafe { (*cursor).cbCertEncoded };
        // A `u32` length always fits a Windows `usize` (32-bit x86 or 64-bit).
        let length = usize::try_from(cb_cert_encoded).expect("certificate length fits usize");
        // SAFETY: `pbCertEncoded`/`cbCertEncoded` describe one encoded
        // certificate owned by the context, so the pair is a valid slice.
        let encoded = unsafe { slice::from_raw_parts(pb_cert_encoded, length) };
        if encoded == expected {
            found = true;
            break;
        }
        // SAFETY: the cursor is non-null and still owned by the enumeration.
        cursor = unsafe { CertEnumCertificatesInStore(store, cursor) };
    }
    if !cursor.is_null() {
        // SAFETY: a non-null cursor is a context this function still owns.
        unsafe { windows_sys::Win32::Security::Cryptography::CertFreeCertificateContext(cursor) };
    }
    close_store(store)?;
    Ok(found)
}

fn open_root_store() -> Result<*mut c_void> {
    let flags =
        CERT_SYSTEM_STORE_LOCAL_MACHINE | CERT_STORE_OPEN_EXISTING_FLAG | CERT_STORE_READONLY_FLAG;
    // SAFETY: `CERT_STORE_PROV_SYSTEM_W` with a null-terminated wide store name
    // and null provider handle opens the LocalMachine Root store read-only.
    let store = unsafe { CertOpenStore(CERT_STORE_PROV_SYSTEM_W, 0, 0, flags, ROOT_STORE.cast()) };
    if store.is_null() {
        return Err(Error::Config(format!(
            "open the Windows LocalMachine Root store: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(store)
}

fn close_store(store: *mut c_void) -> Result<()> {
    // SAFETY: `store` came from `CertOpenStore` and is closed exactly once here.
    let closed = unsafe { CertCloseStore(store, 0) };
    if closed == 0 {
        return Err(Error::Config(format!(
            "close the Windows certificate store: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}
