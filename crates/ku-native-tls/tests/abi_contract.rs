use std::ptr;

use ku_native_tls::*;

#[test]
fn abi_version_build_id_and_status_numbers_are_stable() {
    // SAFETY: Every pointer passed below refers to live local storage.
    unsafe {
        assert_eq!(ku_tls_abi_version(), 1);
        assert_eq!(KU_TLS_STATUS_OK, 0);
        assert_eq!(KU_TLS_STATUS_NULL_POINTER, 1);
        assert_eq!(KU_TLS_STATUS_INVALID_ARGUMENT, 2);
        assert_eq!(KU_TLS_STATUS_LIMIT_EXCEEDED, 3);
        assert_eq!(KU_TLS_STATUS_INVALID_DNS_NAME, 4);
        assert_eq!(KU_TLS_STATUS_INVALID_CA, 5);
        assert_eq!(KU_TLS_STATUS_TLS_ERROR, 6);
        assert_eq!(KU_TLS_STATUS_SESSION_FAILED, 7);
        assert_eq!(KU_TLS_STATUS_TRUNCATED, 8);
        assert_eq!(KU_TLS_STATUS_WOULD_BLOCK, 9);
        assert_eq!(KU_TLS_STATUS_IO_ERROR, 10);
        assert_eq!(KU_TLS_STATUS_PANIC, 255);
        assert_eq!(KU_TLS_MAX_CA_PEM_BYTES, 4 * 1024 * 1024);
        assert_eq!(KU_TLS_MAX_CA_CERTIFICATES, 1024);
        assert_eq!(KU_TLS_MAX_CA_DER_BYTES, 64 * 1024);
        assert_eq!(KU_TLS_MAX_IO_BYTES, 64 * 1024);
        assert_eq!(KU_TLS_MAX_HANDSHAKE_BYTES, 1024 * 1024);
        assert_eq!(KU_TLS_MAX_HANDSHAKE_ITERATIONS, 4096);
        assert_eq!(KU_TLS_MAX_SERVER_NAME_BYTES, 253);
        assert_eq!(KU_TLS_RESUMPTION_CACHE_ENTRIES, 64);

        let mut data = ptr::null();
        let mut len = 0usize;
        assert_eq!(ku_tls_v1_build_id(&mut data, &mut len), KU_TLS_STATUS_OK);
        assert!(!data.is_null());
        let build_id = std::slice::from_raw_parts(data, len);
        assert_eq!(
            build_id,
            b"ku-native-tls/0.1.0;abi=1;rustls=0.23.40;ring=0.17.14;webpki-roots=1.0.7;buffer=65536;handshake=1048576;resumption=64"
        );
    }
}

#[test]
fn null_and_length_validation_happens_before_input_dereference() {
    // SAFETY: Deliberately invalid input addresses are paired with lengths that
    // must be rejected before dereference; all output pointers are live locals.
    unsafe {
        assert_eq!(
            ku_tls_v1_build_id(ptr::null_mut(), ptr::null_mut()),
            KU_TLS_STATUS_NULL_POINTER
        );

        let mut config = ptr::dangling_mut::<KuTlsConfig>();
        assert_eq!(
            ku_tls_v1_config_new(
                KU_TLS_ROOTS_CUSTOM_PEM,
                ptr::dangling::<u8>(),
                KU_TLS_MAX_CA_PEM_BYTES + 1,
                &mut config,
            ),
            KU_TLS_STATUS_LIMIT_EXCEEDED
        );
        assert!(config.is_null());

        assert_eq!(
            ku_tls_v1_config_new(
                KU_TLS_ROOTS_CUSTOM_PEM,
                ptr::dangling::<u8>(),
                usize::MAX,
                ptr::null_mut(),
            ),
            KU_TLS_STATUS_NULL_POINTER
        );

        assert_eq!(
            ku_tls_v1_config_new(KU_TLS_ROOTS_CUSTOM_PEM, ptr::null(), 1, &mut config),
            KU_TLS_STATUS_NULL_POINTER
        );
        assert!(config.is_null());
        assert_eq!(ku_tls_v1_config_drop(ptr::null_mut()), KU_TLS_STATUS_OK);
        assert_eq!(ku_tls_v1_client_drop(ptr::null_mut()), KU_TLS_STATUS_OK);
    }
}

#[test]
fn webpki_and_custom_root_modes_are_unambiguous() {
    // SAFETY: Every non-null pointer refers to live local storage or a live ABI
    // handle and each handle is consumed exactly once.
    unsafe {
        let mut config = ptr::null_mut();
        assert_eq!(
            ku_tls_v1_config_new(KU_TLS_ROOTS_WEBPKI, ptr::null(), 0, &mut config),
            KU_TLS_STATUS_OK
        );
        assert!(!config.is_null());

        let mut session = ptr::dangling_mut::<KuTlsClientSession>();
        assert_eq!(
            ku_tls_v1_client_new(
                config,
                ptr::dangling::<u8>(),
                KU_TLS_MAX_SERVER_NAME_BYTES + 1,
                &mut session,
            ),
            KU_TLS_STATUS_LIMIT_EXCEEDED
        );
        assert!(session.is_null());
        assert_eq!(
            ku_tls_v1_client_new(config, ptr::null(), 1, &mut session),
            KU_TLS_STATUS_NULL_POINTER
        );
        assert!(session.is_null());
        assert_eq!(
            ku_tls_v1_client_new(config, b"bad name".as_ptr(), 8, &mut session),
            KU_TLS_STATUS_INVALID_DNS_NAME
        );
        assert!(session.is_null());
        let mut state = 99;
        assert_eq!(
            ku_tls_v1_client_wants_read(ptr::null(), &mut state),
            KU_TLS_STATUS_NULL_POINTER
        );
        assert_eq!(state, 0);
        assert_eq!(ku_tls_v1_config_drop(config), KU_TLS_STATUS_OK);

        let ignored = b"ignored";
        assert_eq!(
            ku_tls_v1_config_new(
                KU_TLS_ROOTS_WEBPKI,
                ignored.as_ptr(),
                ignored.len(),
                &mut config,
            ),
            KU_TLS_STATUS_INVALID_ARGUMENT
        );
        assert!(config.is_null());
        assert_eq!(
            ku_tls_v1_config_new(KU_TLS_ROOTS_CUSTOM_PEM, ptr::null(), 0, &mut config),
            KU_TLS_STATUS_INVALID_CA
        );
        assert!(config.is_null());
    }
}
