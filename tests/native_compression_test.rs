//! Public compression contracts and optional real-library regressions.
//! A passing source check is deliberately separate from native execution.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::cli::check_source;
use native_harness::{emit_c, run_bounded, OutputLimits, TempDir};
use std::{env, fs, path::PathBuf, process::Command, time::Duration};

const SOURCE: &str = r#"
import {bytes, zstd, gzip, deflate} from "std"
fn main(): null! {
    source = bytes.from_array([0, 1, 127, 128, 255])?
    z = zstd.compress(source, 4096)?
    g = gzip.compress(source, 4096)?
    d = deflate.compress(source, 4096)?
    if (source.len() != 5 || source.get(4)? != 255) { fail "codec consumed its input" }
    round_z = zstd.decompress(z, 5)?
    round_g = gzip.decompress(g, 5)?
    round_d = deflate.decompress(d, 5)?
    if (round_z.get(3)? != 128 || round_g.get(4)? != 255 || round_d.get(0)? != 0) {
        fail "binary roundtrip failed"
    }
    if (z.len() == 0 || g.len() == 0 || d.len() == 0) { fail "owned encoding disappeared" }
    text = bytes.from_str("中文😀")?
    encoded = gzip.compress(text, 4096)?
    decoded = gzip.decompress(encoded, 4096)?
    if (decoded.to_str()? != "中文😀") { fail "Unicode roundtrip failed" }
    limited = false
    try { gzip.decompress(g, 4)? } catch(error) { limited = error.code == "output_limit" }
    if (!limited) { fail "output bound was not enforced" }
    empty = bytes.from_str("")?
    ez = zstd.compress(empty, 256)?
    eg = gzip.compress(empty, 256)?
    ed = deflate.compress(empty, 256)?
    rz = zstd.decompress(ez, 0)?
    rg = gzip.decompress(eg, 0)?
    rd = deflate.decompress(ed, 0)?
    if (rz.len() != 0 || rg.len() != 0 || rd.len() != 0) { fail "empty frame failed" }
    i = 0
    while (i < 32) {
        item = zstd.decompress(z, 5)?
        if (item.len() != 5) { fail "reused bytes changed" }
        i = i + 1
    }
    println("native-compression-public-roundtrip-ok")
    return ok(null)
}
"#;

#[test]
fn native_compression_signatures_keep_readonly_inputs_and_explicit_limits() {
    check_source("compression-contract.ku", SOURCE).expect("compression contract checks");
    for (label, source) in [
        (
            "missing limit",
            r#"import {bytes,zstd} from "std" fn main():null! { b=bytes.from_array([1])? zstd.compress(b)? return ok(null) }"#,
        ),
        (
            "non-byte input",
            r#"import gzip from "std.gzip" fn main():null! { gzip.compress("text",20)? return ok(null) }"#,
        ),
        (
            "non-integer limit",
            r#"import {bytes,deflate} from "std" fn main():null! { b=bytes.from_array([1])? deflate.decompress(b,true)? return ok(null) }"#,
        ),
    ] {
        assert!(
            check_source("compression-invalid.ku", source).is_err(),
            "accepted {label}"
        );
    }
}

#[test]
fn native_unused_compression_has_no_headers_feature_markers_or_contexts() {
    let directory = TempDir::new("compression-unused");
    let generated = emit_c(directory.path(), "fn main() { println(17) }");
    for unexpected in [
        "#define KU_FEATURE_ZSTD 1",
        "#define KU_FEATURE_ZLIB 1",
        "#include <zstd.h>",
        "#include <zlib.h>",
        "ku_compression_allocate",
    ] {
        assert!(
            !generated.contains(unexpected),
            "unused codec emitted {unexpected}"
        );
    }
}

fn ku_binary() -> PathBuf {
    env::var_os("KU_BIN")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_BIN_EXE_ku").map(PathBuf::from))
        .expect("build Ku or set KU_BIN for public native compression regression")
}

fn assert_task_codec_rejected_before_c(label: &str, source: &str) {
    // First prove this is valid source. A parser/type error is not evidence
    // that a native Task cannot reach an unbudgeted synchronous codec.
    check_source("task-codec-boundary.ku", source)
        .unwrap_or_else(|error| panic!("{label} is not valid Ku source:\n{error}\n{source}"));
    let directory = TempDir::new(label);
    fs::write(directory.path().join("main.ku"), source).unwrap();
    let mut command = Command::new(ku_binary());
    command
        .current_dir(directory.path())
        .args(["build", "--native", "main.ku"]);
    let output = run_bounded(
        &mut command,
        Duration::from_secs(120),
        OutputLimits::new(4 * 1024 * 1024, 8 * 1024 * 1024),
    )
    .expect("native Task codec rejection is bounded");
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "{label} allowed an unbudgeted Task codec:\n{diagnostics}"
    );
    assert!(
        diagnostics.contains("native async subset does not support"),
        "{label} failed for the wrong reason:\n{diagnostics}"
    );
    assert!(
        !directory.path().join("main.c").exists(),
        "{label} wrote C before rejection"
    );
    assert!(
        !directory
            .path()
            .join(if cfg!(windows) { "main.exe" } else { "main" })
            .exists(),
        "{label} wrote an executable before rejection"
    );
}

#[test]
fn native_compression_task_direct_and_sync_helper_calls_fail_before_c_artifacts() {
    for codec in ["zstd", "gzip", "deflate"] {
        let imports = format!("import {{bytes,{codec}}} from \"std\"\n");
        let operation = format!("input=bytes.from_array([1,0,255])? {codec}.compress(input,100)?");
        let helper = format!("fn Compress():null! {{ {operation} return ok(null) }}\n");
        assert_task_codec_rejected_before_c(
            &format!("{codec}-task-direct"),
            &format!("{imports}async fn main():null! {{ {operation} return ok(null) }}"),
        );
        assert_task_codec_rejected_before_c(
            &format!("{codec}-task-helper"),
            &format!("{imports}{helper}async fn main():null! {{ Compress()? return ok(null) }}"),
        );
        // Mixed HTTP lowering filters setup functions and std capabilities.
        // It must not let either the direct call or a setup-side sync helper
        // escape into an async handler without Task resource adaptation.
        let setup = "fn main():null! { app=http.server() app.get(\"/\",Handle) app.listen(\"127.0.0.1:0\")? return ok(null) }";
        assert_task_codec_rejected_before_c(&format!("{codec}-handler-direct"),
            &format!("{imports}import http from \"std.http\"\nasync fn Handle():HttpResponse! {{ {operation} return ok(http.text(\"ok\")) }}\n{setup}"));
        assert_task_codec_rejected_before_c(&format!("{codec}-handler-helper"),
            &format!("{imports}import http from \"std.http\"\n{helper}async fn Handle():HttpResponse! {{ Compress()? return ok(http.text(\"ok\")) }}\n{setup}"));
    }
}

#[test]
fn native_compression_task_boundary_preserves_supported_bytes_without_codecs() {
    let directory = TempDir::new("compression-task-no-codec");
    let generated = emit_c(
        directory.path(),
        r#"
import bytes from "std.bytes"
async fn main():null! {
    input=bytes.from_array([65,0,255])?
    if(input.len()!=3 || input.get(2)?!=255) { fail "Task bytes changed" }
    return ok(null)
}
"#,
    );
    assert!(
        generated.contains("ku_task_driver_"),
        "positive fixture did not select the real Task path"
    );
    for unexpected in [
        "#define KU_FEATURE_ZSTD 1",
        "#define KU_FEATURE_ZLIB 1",
        "#include <zstd.h>",
        "#include <zlib.h>",
    ] {
        assert!(
            !generated.contains(unexpected),
            "unused codec added {unexpected}"
        );
    }
}

#[test]
#[ignore = "requires target-matched Zstandard 1.5.7 and zlib 1.4 optional native libraries/headers"]
fn native_compression_public_project_runs_outside_source_with_space_and_unicode() {
    let directory = TempDir::new("compression-public");
    let project = directory.path().join("codec 中文 project");
    fs::create_dir(&project).expect("create isolated Unicode project path");
    fs::write(project.join("入口 中文.ku"), SOURCE).expect("write public compression example");
    let deployment = directory.path().join("另外的 输出目录");
    fs::create_dir(&deployment).expect("create separate Unicode output directory");
    let executable = deployment.join(if cfg!(windows) {
        "程序 中文.exe"
    } else {
        "程序 中文"
    });
    let mut build = Command::new(ku_binary());
    build
        .current_dir(&project)
        .args([
            "build",
            "--backend",
            "c",
            "--release",
            "--verbose",
            "入口 中文.ku",
            "-o",
        ])
        .arg(&executable);
    let output = run_bounded(
        &mut build,
        Duration::from_secs(120),
        OutputLimits::new(4 * 1024 * 1024, 8 * 1024 * 1024),
    )
    .expect("public native compression build is bounded");
    assert!(
        output.status.success(),
        "public native compression build failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_file(project.join("入口 中文.ku")).expect("remove original source before execution");
    let mut run = Command::new(&executable);
    run.current_dir(directory.path());
    let output = run_bounded(
        &mut run,
        Duration::from_secs(15),
        OutputLimits::new(1024 * 1024, 2 * 1024 * 1024),
    )
    .expect("public native compression runtime is bounded");
    assert!(
        output.status.success(),
        "native compression program failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "native-compression-public-roundtrip-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const C_CONTRACT: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
#define CHECK(c) do { if(!(c)) { fprintf(stderr,"codec contract line %d: %s\n",__LINE__,#c); return 91; } } while(0)
typedef KuResult_bytes (*CodecCall)(KuBytes,int64_t);
typedef struct Codec { CodecCall encode,decode; int members,checksum; } Codec;
static int code_is(KuResult_bytes* value,const char* name) {
  return !value->ok && value->error.code.len==strlen(name)
      && !memcmp(value->error.code.ptr,name,strlen(name));
}
static void drop_result(KuResult_bytes* value) {
  if(value->ok)ku_drop_bytes(&value->value);
  else ku_error_drop(&value->error);
  *value=(KuResult_bytes){0};
}
int main(void) {
  static uint8_t data[65536]; uint32_t seed=0x64539a71u;
  for(size_t i=0;i<sizeof(data);i++){seed^=seed<<13;seed^=seed>>17;seed^=seed<<5;data[i]=(uint8_t)seed;}
  KuBytes input={data,sizeof(data),sizeof(data),KU_BYTES_STATIC};
  Codec codecs[]={{ku_zstd_compress,ku_zstd_decompress,1,1},
      {ku_gzip_compress,ku_gzip_decompress,1,1},{ku_deflate_compress,ku_deflate_decompress,0,0}};
  for(size_t which=0;which<sizeof(codecs)/sizeof(codecs[0]);which++) {
    Codec codec=codecs[which]; size_t before=ku_perf_calls;
    KuResult_bytes encoded=codec.encode(input,200000);
    CHECK(encoded.ok && encoded.value.len && !memcmp(input.ptr,data,sizeof(data)));
    size_t attempts=ku_codec_fail_attempts;CHECK(attempts && ku_perf_calls>before);
    size_t held_allocations=ku_perf_live_allocations,held_bytes=ku_perf_live_bytes;
    KuResult_bytes decoded=codec.decode(encoded.value,sizeof(data));
    CHECK(decoded.ok && decoded.value.len==sizeof(data) && !memcmp(decoded.value.ptr,data,sizeof(data)));drop_result(&decoded);
    decoded=codec.decode(encoded.value,sizeof(data)-1u);CHECK(code_is(&decoded,"output_limit"));drop_result(&decoded);
    size_t cuts[]={0u,encoded.value.len/2u,encoded.value.len-1u};
    for(size_t cut=0;cut<3u;cut++) {
      KuBytes partial=encoded.value;partial.len=cuts[cut];
      decoded=codec.decode(partial,sizeof(data));CHECK(!decoded.ok);drop_result(&decoded);
    }
    if(codec.checksum) {
      encoded.value.ptr[encoded.value.len-1u]^=1u;
      decoded=codec.decode(encoded.value,sizeof(data));CHECK(!decoded.ok);drop_result(&decoded);
      encoded.value.ptr[encoded.value.len-1u]^=1u;
    }
    uint8_t* multiple=(uint8_t*)malloc(encoded.value.len*2u+1u);CHECK(multiple);
    memcpy(multiple,encoded.value.ptr,encoded.value.len);
    memcpy(multiple+encoded.value.len,encoded.value.ptr,encoded.value.len);
    KuBytes joined={multiple,encoded.value.len*2u,encoded.value.len*2u+1u,KU_BYTES_STATIC};
    decoded=codec.decode(joined,sizeof(data)*2u);
    if(codec.members) {
      CHECK(decoded.ok && decoded.value.len==sizeof(data)*2u);
      CHECK(!memcmp(decoded.value.ptr,data,sizeof(data)) && !memcmp(decoded.value.ptr+sizeof(data),data,sizeof(data)));
    } else CHECK(code_is(&decoded,"invalid_data"));
    drop_result(&decoded);
    multiple[encoded.value.len]=0x7fu;joined.len=encoded.value.len+1u;
    decoded=codec.decode(joined,sizeof(data)*2u);CHECK(!decoded.ok);drop_result(&decoded);free(multiple);
    CHECK(ku_perf_live_allocations==held_allocations && ku_perf_live_bytes==held_bytes);
    for(size_t failure=1u;failure<=attempts;failure++) {
      ku_codec_fail_attempts=0;ku_codec_fail_at=failure;
      KuResult_bytes failed=codec.encode(input,200000);
      ku_codec_fail_at=0;CHECK(code_is(&failed,"out_of_memory"));drop_result(&failed);
      CHECK(ku_perf_live_allocations==held_allocations && ku_perf_live_bytes==held_bytes);
    }
    for(size_t repeat=0;repeat<8u;repeat++) {
      decoded=codec.decode(encoded.value,sizeof(data));CHECK(decoded.ok);drop_result(&decoded);
    }
    decoded=codec.decode(encoded.value,-1);CHECK(code_is(&decoded,"invalid_argument"));drop_result(&decoded);
    decoded=codec.decode(encoded.value,67108865);CHECK(code_is(&decoded,"invalid_argument"));drop_result(&decoded);
    drop_result(&encoded);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
    ku_codec_fail_attempts=0;
  }
  puts("compression-bounds-members-failures-cleanup-ok");return 0;
}
"#;

#[test]
#[ignore = "requires explicit KU_COMPRESSION_TEST_LINK_ARGS for frozen target-matched codec headers and libraries"]
fn native_compression_bounds_members_and_allocation_failures_release_owned_storage() {
    // The C ledger covers owned output and zlib's public allocator callbacks.
    // Zstandard's opaque public-API allocations remain inside the native
    // library; these counters do not claim to measure all of its memory.
    let directory = TempDir::new("compression-failure-contract");
    let generated = emit_c(directory.path(), SOURCE);
    let hook = format!(
        "{}\n{}",
        native_allocation_harness::ALLOCATION_HOOK,
        r#"
static size_t ku_codec_fail_at,ku_codec_fail_attempts;
#undef malloc
#undef calloc
#undef realloc
static void* ku_codec_malloc(size_t n) {
  if(++ku_codec_fail_attempts==ku_codec_fail_at)return NULL;
  return ku_perf_malloc(n);
}
static void* ku_codec_calloc(size_t n,size_t s) {
  if(++ku_codec_fail_attempts==ku_codec_fail_at)return NULL;
  return ku_perf_calloc(n,s);
}
static void* ku_codec_realloc(void* p,size_t n) {
  if(++ku_codec_fail_attempts==ku_codec_fail_at)return NULL;
  return ku_perf_realloc(p,n);
}
#define malloc ku_codec_malloc
#define calloc ku_codec_calloc
#define realloc ku_codec_realloc
"#
    );
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let mut instrumented = generated.replacen(
        "typedef struct KuString {",
        &format!("{hook}\ntypedef struct KuString {{"),
        1,
    );
    instrumented = instrumented.replacen(
        "int main(void) {",
        "static int original_project_main(void) {",
        1,
    );
    instrumented.push_str(C_CONTRACT);
    let source = directory.path().join("contract.c");
    fs::write(&source, instrumented).expect("write bounded codec failure fixture");
    let executable = directory.path().join(if cfg!(windows) {
        "contract.exe"
    } else {
        "contract"
    });
    let mut command =
        Command::new(env::var_os("KU_COMPRESSION_TEST_CC").unwrap_or_else(|| "clang".into()));
    command
        .arg(&source)
        .args(["-std=c11", "-O2", "-o"])
        .arg(&executable);
    // Newline-delimited argv, not a shell command. Paths may contain spaces.
    // Required opt-in prevents an unrelated system zlib from being selected.
    let flags = env::var("KU_COMPRESSION_TEST_LINK_ARGS")
        .expect("set explicit target codec -I arguments and absolute archive paths");
    for flag in flags.lines().filter(|flag| !flag.is_empty()) {
        command.arg(flag);
    }
    let output = run_bounded(
        &mut command,
        Duration::from_secs(120),
        OutputLimits::new(4 * 1024 * 1024, 8 * 1024 * 1024),
    )
    .expect("codec failure fixture compile is bounded");
    assert!(
        output.status.success(),
        "codec fixture compile failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_file(&source).expect("remove temporary instrumented codec C");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        Duration::from_secs(30),
        OutputLimits::new(1024 * 1024, 2 * 1024 * 1024),
    )
    .expect("codec failure fixture execution is bounded");
    assert!(
        output.status.success(),
        "codec failure fixture failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "compression-bounds-members-failures-cleanup-ok\n"
    );
    assert!(output.stderr.is_empty());
}
