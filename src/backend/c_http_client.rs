//! Optional synchronous HTTP transport. Task outbound calls stay unsupported.
use super::{
    c_expr, c_place_expr, c_value_expr, output::COutput, program_uses_intrinsic, unsupported,
    IrExpr, IrProgram, KuResult,
};
pub(super) fn uses_runtime(p: &IrProgram) -> bool {
    ["http.get", "http.post", "http.request"]
        .iter()
        .any(|name| program_uses_intrinsic(p, name))
}
pub(super) fn emit(out: &mut COutput, p: &IrProgram) {
    if !uses_runtime(p) && !program_uses_intrinsic(p, "http.client") {
        return;
    }
    if uses_runtime(p) {
        out.push_str("#define KU_FEATURE_NATIVE_HTTP_CLIENT 1\n#define KuHttpClientSliceV1 __ku_http_client_slice_v1\n#define KuHttpClientHeaderV1 __ku_http_client_header_v1\n#define KuHttpClientInputV1 __ku_http_client_input_v1\n#define KuHttpClientOutputV1 __ku_http_client_output_v1\n");
        out.push_str(include_str!(
            "../../crates/ku-native-http-client/include/ku_native_http_client.h"
        ));
    }
    out.push_str(include_str!("c_http_client_runtime.c"));
    out.push_str("\n#undef KuHttpClientSliceV1\n#undef KuHttpClientHeaderV1\n#undef KuHttpClientInputV1\n#undef KuHttpClientOutputV1\n#undef KU_NATIVE_HTTP_CLIENT_V1_H\n");
}
pub(super) fn intrinsic_expr(name: &str, args: &[IrExpr]) -> KuResult<Option<String>> {
    let call = match name {
        "__ku_http_response_from_object_owned" => format!(
            "__ku_http_response_from_object_owned({})",
            c_value_expr(&args[0])?
        ),
        "__ku_http_response_from_object_view" => {
            format!("__ku_http_response_from_object_view({})", c_expr(&args[0])?)
        }
        "__ku_http_client_config_field_int" | "__ku_http_client_config_field_str" => {
            format!("{name}({}, {})", c_expr(&args[0])?, c_expr(&args[1])?)
        }
        "__ku_http_response_public_value" => {
            format!("__ku_http_response_public_value({})", c_expr(&args[0])?)
        }
        "__ku_http_headers_get" => format!(
            "__ku_http_headers_get({}, {})",
            c_expr(&args[0])?,
            c_expr(&args[1])?
        ),
        "__ku_http_response_headers_read" => {
            format!("__ku_http_response_headers_read({})", c_expr(&args[0])?)
        }
        "__ku_http_response_headers_prepare" => format!(
            "(__ku_http_response_headers_mut(&({})), (uint8_t)0)",
            c_place_expr(&args[0])?
        ),
        "http.get" if args.len() == 1 => format!("__ku_http_client_get({})", c_expr(&args[0])?),
        "http.post" if args.len() == 2 => format!(
            "__ku_http_client_post({}, {})",
            c_expr(&args[0])?,
            c_expr(&args[1])?
        ),
        "http.request" if args.len() == 1 => {
            format!("__ku_http_client_request({})", c_expr(&args[0])?)
        }
        "http.client" if args.len() <= 1 => format!(
            "__ku_http_client_config({})",
            args.first()
                .map(c_expr)
                .transpose()?
                .unwrap_or("NULL".into())
        ),
        "http.get" | "http.post" | "http.request" | "http.client" => {
            return Err(unsupported("invalid native HTTP client signature"))
        }
        _ => return Ok(None),
    };
    Ok(Some(call))
}
