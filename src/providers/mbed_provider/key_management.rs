// Copyright 2020 Contributors to the Parsec project.
// SPDX-License-Identifier: Apache-2.0
use super::MbedProvider;
use crate::authenticators::ApplicationName;
use crate::key_info_managers;
use crate::key_info_managers::{KeyInfo, KeyTriple, ManageKeyInfo};
use log::error;
use log::{info, warn};
use parsec_interface::operations::psa_key_attributes::Attributes;
use parsec_interface::operations::{
    psa_destroy_key, psa_export_public_key, psa_generate_key, psa_import_key,
};
use parsec_interface::requests::{ProviderID, ResponseStatus, Result};
use psa_crypto::operations::key_management as psa_crypto_key_management;
use psa_crypto::types::key;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// Gets a PSA Key ID from the Key Info Manager.
/// Wrapper around the get method of the Key Info Manager to convert the key ID to the psa_key_id_t
/// type.
pub fn get_key_id(
    key_triple: &KeyTriple,
    store_handle: &dyn ManageKeyInfo,
) -> Result<key::psa_key_id_t> {
    match store_handle.get(key_triple) {
        Ok(Some(key_info)) => {
            if key_info.id.len() == 4 {
                let mut dst = [0; 4];
                dst.copy_from_slice(&key_info.id);
                Ok(u32::from_ne_bytes(dst))
            } else {
                format_error!(
                    "Stored Key ID is not valid.",
                    ResponseStatus::KeyInfoManagerError
                );
                Err(ResponseStatus::KeyInfoManagerError)
            }
        }
        Ok(None) => Err(ResponseStatus::PsaErrorDoesNotExist),
        Err(string) => Err(key_info_managers::to_response_status(string)),
    }
}

/// Creates a new PSA Key ID and stores it in the Key Info Manager.
fn create_key_id(
    key_triple: KeyTriple,
    key_attributes: Attributes,
    store_handle: &mut dyn ManageKeyInfo,
    max_current_id: &AtomicU32,
) -> Result<key::psa_key_id_t> {
    // fetch_add adds 1 to the old value and returns the old value, so add 1 to local value for new ID
    let new_key_id = max_current_id.fetch_add(1, Relaxed) + 1;
    if new_key_id > key::PSA_KEY_ID_USER_MAX {
        // If storing key failed and no other keys were created in the mean time, it is safe to
        // decrement the key counter.
        let _ = max_current_id.store(key::PSA_KEY_ID_USER_MAX, Relaxed);
        error!(
            "PSA max key ID limit of {} reached",
            key::PSA_KEY_ID_USER_MAX
        );
        return Err(ResponseStatus::PsaErrorInsufficientMemory);
    }

    let key_info = KeyInfo {
        id: new_key_id.to_ne_bytes().to_vec(),
        attributes: key_attributes,
    };
    match store_handle.insert(key_triple.clone(), key_info) {
        Ok(insert_option) => {
            if insert_option.is_some() {
                warn!("Overwriting Key triple mapping ({})", key_triple);
            }
            Ok(new_key_id)
        }
        Err(string) => Err(key_info_managers::to_response_status(string)),
    }
}

fn remove_key_id(key_triple: &KeyTriple, store_handle: &mut dyn ManageKeyInfo) -> Result<()> {
    // ID Counter not affected as overhead and extra complication deemed unnecessary
    match store_handle.remove(key_triple) {
        Ok(_) => Ok(()),
        Err(string) => Err(key_info_managers::to_response_status(string)),
    }
}

pub fn key_info_exists(key_triple: &KeyTriple, store_handle: &dyn ManageKeyInfo) -> Result<bool> {
    store_handle
        .exists(key_triple)
        .or_else(|e| Err(key_info_managers::to_response_status(e)))
}

impl MbedProvider {
    pub(super) fn psa_generate_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_generate_key::Operation,
    ) -> Result<psa_generate_key::Result> {
        info!("Mbed Provider - Create Key");
        let key_name = op.key_name;
        let key_attributes = op.attributes;
        let key_triple = KeyTriple::new(app_name, ProviderID::MbedCrypto, key_name);
        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        if key_info_exists(&key_triple, &*store_handle)? {
            return Err(ResponseStatus::PsaErrorAlreadyExists);
        }
        let key_id = create_key_id(
            key_triple.clone(),
            key_attributes,
            &mut *store_handle,
            &self.id_counter,
        )?;

        let _guard = self
            .key_handle_mutex
            .lock()
            .expect("Grabbing key handle mutex failed");

        match psa_crypto_key_management::generate(key_attributes, Some(key_id)) {
            Ok(_) => Ok(psa_generate_key::Result {}),
            Err(error) => {
                remove_key_id(&key_triple, &mut *store_handle)?;
                let error = ResponseStatus::from(error);
                format_error!("Generate key status: {}", error);
                Err(error)
            }
        }
    }

    pub(super) fn psa_import_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_import_key::Operation,
    ) -> Result<psa_import_key::Result> {
        info!("Mbed Provider - Import Key");
        let key_name = op.key_name;
        let key_attributes = op.attributes;
        let key_data = op.data;
        let key_triple = KeyTriple::new(app_name, ProviderID::MbedCrypto, key_name);
        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        if key_info_exists(&key_triple, &*store_handle)? {
            return Err(ResponseStatus::PsaErrorAlreadyExists);
        }
        let key_id = create_key_id(
            key_triple.clone(),
            key_attributes,
            &mut *store_handle,
            &self.id_counter,
        )?;

        let _guard = self
            .key_handle_mutex
            .lock()
            .expect("Grabbing key handle mutex failed");

        match psa_crypto_key_management::import(key_attributes, Some(key_id), &key_data[..]) {
            Ok(_) => Ok(psa_import_key::Result {}),
            Err(error) => {
                remove_key_id(&key_triple, &mut *store_handle)?;
                let error = ResponseStatus::from(error);
                format_error!("Import key status: {}", error);
                Err(error)
            }
        }
    }

    pub(super) fn psa_export_public_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_export_public_key::Operation,
    ) -> Result<psa_export_public_key::Result> {
        info!("Mbed Provider - Export Public Key");
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::MbedCrypto, key_name);
        let store_handle = self.key_info_store.read().expect("Key store lock poisoned");
        let key_id = get_key_id(&key_triple, &*store_handle)?;

        let _guard = self
            .key_handle_mutex
            .lock()
            .expect("Grabbing key handle mutex failed");

        let id = key::Id::from_persistent_key_id(key_id);
        let key_attributes = key::Attributes::from_key_id(id)?;
        let buffer_size = key_attributes.export_key_output_size()?;
        let mut buffer = vec![0u8; buffer_size];

        let export_length = psa_crypto_key_management::export_public(id, &mut buffer)?;

        buffer.resize(export_length, 0);
        Ok(psa_export_public_key::Result { data: buffer })
    }

    pub(super) fn psa_destroy_key_internal(
        &self,
        app_name: ApplicationName,
        op: psa_destroy_key::Operation,
    ) -> Result<psa_destroy_key::Result> {
        info!("Mbed Provider - Destroy Key");
        let key_name = op.key_name;
        let key_triple = KeyTriple::new(app_name, ProviderID::MbedCrypto, key_name);
        let mut store_handle = self
            .key_info_store
            .write()
            .expect("Key store lock poisoned");
        let key_id = get_key_id(&key_triple, &*store_handle)?;

        let _guard = self
            .key_handle_mutex
            .lock()
            .expect("Grabbing key handle mutex failed");
        let destroy_key_status;

        // Safety:
        //   * at this point the provider has been instantiated so Mbed Crypto has been initialized
        //   * self.key_handle_mutex prevents concurrent accesses
        //   * self.key_slot_semaphore prevents overflowing key slots
        let id = key::Id::from_persistent_key_id(key_id);
        unsafe {
            destroy_key_status = psa_crypto_key_management::destroy(id);
        }

        match destroy_key_status {
            Ok(()) => {
                remove_key_id(&key_triple, &mut *store_handle)?;
                Ok(psa_destroy_key::Result {})
            }
            Err(error) => {
                let error = ResponseStatus::from(error);
                format_error!("Destroy key status: {}", error);
                Err(error)
            }
        }
    }
}
