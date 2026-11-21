//! Locale-independent C string transforms from the same Unicode implementation
//! as the interpreter. Tables are built once, only when a program uses casing.
use std::{fmt::Write, sync::OnceLock};

use super::output::COutput;
use crate::error::KuResult;

type Mapping = Vec<(u32, Vec<u32>)>;
type Ranges = Vec<(u32, u32, u8)>;

struct Tables {
    lower: Mapping,
    upper: Mapping,
    context: Ranges,
    whitespace: Ranges,
}

fn range_push(ranges: &mut Ranges, cp: u32, kind: u8) {
    if kind == 0 {
        return;
    }
    if let Some((_, last, previous)) = ranges.last_mut() {
        if *last + 1 == cp && *previous == kind {
            *last = cp;
            return;
        }
    }
    ranges.push((cp, cp, kind));
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut tables = Tables {
            lower: Vec::new(),
            upper: Vec::new(),
            context: Vec::new(),
            whitespace: Vec::new(),
        };
        for cp in 0..=0x10ffff {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            for (mapping, value) in [
                (&mut tables.lower, c.to_lowercase().collect::<Vec<_>>()),
                (&mut tables.upper, c.to_uppercase().collect::<Vec<_>>()),
            ] {
                if value.as_slice() != [c] {
                    mapping.push((cp, value.into_iter().map(u32::from).collect()));
                }
            }
            range_push(&mut tables.whitespace, cp, u8::from(c.is_whitespace()));
            // Stable Rust does not expose Case_Ignorable/Cased. Ask its full
            // string conversion in two contexts instead of importing another
            // Unicode version or using unstable APIs. Overlapping properties
            // (e.g. U+0345) must be classified as ignorable first.
            let preceding = format!("A{c}Σ.").to_lowercase().ends_with("ς.");
            let following = format!("AΣ{c}.").to_lowercase().chars().nth(1) == Some('σ');
            let class = match (preceding, following) {
                (true, false) => 1, // Case_Ignorable, including cased overlaps.
                (true, true) => 2,  // Non-ignorable cased character.
                (false, false) => 0,
                (false, true) => unreachable!("inconsistent Unicode sigma contexts"),
            };
            range_push(&mut tables.context, cp, class);
        }
        tables
    })
}

fn emit_ranges(out: &mut COutput, name: &str, ranges: &Ranges) {
    out.push_str(&format!("static const KuUnicodeRange {name}[]={{\n"));
    for &(first, last, kind) in ranges {
        out.push_str(&format!("  {{{first}u,{last}u,{kind}u}},\n"));
    }
    out.push_str("};\n");
}

fn emit_mapping(out: &mut COutput, name: &str, mapping: &Mapping) {
    let mut values: Vec<u32> = Vec::new();
    out.push_str(&format!("static const KuUnicodeMap {name}[]={{\n"));
    for (cp, result) in mapping {
        out.push_str(&format!(
            "  {{{cp}u,{}u,{}u}},\n",
            values.len(),
            result.len()
        ));
        values.extend(result.iter().copied());
    }
    out.push_str("};\n");
    let mut text = format!("static const uint32_t {name}_values[]={{\n");
    for chunk in values.chunks(16) {
        for value in chunk {
            write!(&mut text, "{value}u,").expect("writing Unicode scalar");
        }
        text.push('\n');
    }
    text.push_str("};\n");
    out.push_str(&text);
}

pub(super) fn emit(out: &mut COutput, trim: bool, casing: bool) -> KuResult<()> {
    if !trim && !casing {
        return Ok(());
    }
    out.check()?;
    let t = tables();
    out.push_str(&format!(
        "/* Rust Unicode {}.{}.{}; no locale or runtime library. */\n",
        char::UNICODE_VERSION.0,
        char::UNICODE_VERSION.1,
        char::UNICODE_VERSION.2
    ));
    out.push_str(BASE);
    if trim {
        emit_ranges(out, "ku_unicode_space", &t.whitespace);
        out.push_str(TRIM);
    }
    if casing {
        emit_ranges(out, "ku_unicode_context", &t.context);
        emit_mapping(out, "ku_unicode_lower", &t.lower);
        emit_mapping(out, "ku_unicode_upper", &t.upper);
        out.push_str(CASE);
    }
    out.check()
}

const BASE: &str = include_str!("c_string_unicode_base.c");

const TRIM: &str = include_str!("c_string_unicode_trim.c");

const CASE: &str = r#"
static const KuUnicodeMap* ku_unicode_mapping(const KuUnicodeMap* table,size_t count,uint32_t cp) {
  size_t lo=0,hi=count;
  while(lo<hi){size_t mid=lo+(hi-lo)/2u;if(table[mid].codepoint<cp)lo=mid+1u;else hi=mid;}
  return lo<count&&table[lo].codepoint==cp?table+lo:NULL;
}
static size_t ku_unicode_width(uint32_t cp){return cp<0x80u?1u:cp<0x800u?2u:cp<0x10000u?3u:4u;}
static void ku_unicode_write(uint8_t* out,size_t* at,uint32_t cp){
  if(cp<0x80u)out[(*at)++]=(uint8_t)cp;
  else if(cp<0x800u){out[(*at)++]=(uint8_t)(0xc0u|(cp>>6));out[(*at)++]=(uint8_t)(0x80u|(cp&63u));}
  else if(cp<0x10000u){out[(*at)++]=(uint8_t)(0xe0u|(cp>>12));out[(*at)++]=(uint8_t)(0x80u|((cp>>6)&63u));out[(*at)++]=(uint8_t)(0x80u|(cp&63u));}
  else{out[(*at)++]=(uint8_t)(0xf0u|(cp>>18));out[(*at)++]=(uint8_t)(0x80u|((cp>>12)&63u));out[(*at)++]=(uint8_t)(0x80u|((cp>>6)&63u));out[(*at)++]=(uint8_t)(0x80u|(cp&63u));}
}
static KuString ku_string_unicode_case(KuString s,int upper) {
  const KuUnicodeMap* table=upper?ku_unicode_upper:ku_unicode_lower;
  size_t count=upper?sizeof(ku_unicode_upper)/sizeof(*ku_unicode_upper):sizeof(ku_unicode_lower)/sizeof(*ku_unicode_lower);
  const uint32_t* values=upper?ku_unicode_upper_values:ku_unicode_lower_values;
  KuString result={0};size_t length=0;
  for(unsigned pass=0;pass<2u;pass++){
    size_t at=0,out=0,fuel=0;int preceded=0;
    while(at<s.len){
      if(!ku_unicode_continue(&fuel)){ku_string_drop(&result);return (KuString){0};}
      uint32_t cp=ku_unicode_read(s,&at),single=cp;
      const KuUnicodeMap* mapping=ku_unicode_mapping(table,count,cp);
      const uint32_t* mapped=mapping?values+mapping->offset:&single;
      size_t mapped_count=mapping?mapping->length:1u;
      if(!upper&&cp==0x3a3u&&preceded){
        size_t look=at;int followed=0;
        while(look<s.len){
          if(!ku_unicode_continue(&fuel)){ku_string_drop(&result);return (KuString){0};}
          uint32_t next=ku_unicode_read(s,&look);
          unsigned kind=ku_unicode_class(ku_unicode_context,sizeof(ku_unicode_context)/sizeof(*ku_unicode_context),next);
          if(kind==1u)continue;followed=kind==2u;break;
        }
        if(!followed){single=0x3c2u;mapped=&single;mapped_count=1u;}
      }
      for(size_t k=0;k<mapped_count;k++){
        if(!pass)length=ku_size_add(length,ku_unicode_width(mapped[k]),"Unicode string result");
        else ku_unicode_write(result.ptr,&out,mapped[k]);
      }
      unsigned kind=ku_unicode_class(ku_unicode_context,sizeof(ku_unicode_context)/sizeof(*ku_unicode_context),cp);
      if(kind!=1u)preceded=kind==2u;
    }
    if(!pass)result=ku_string_alloc(length);
  }
  return result;
}
static KuString ku_string_unicode_lower(KuString s){return ku_string_unicode_case(s,0);}
static KuString ku_string_unicode_upper(KuString s){return ku_string_unicode_case(s,1);}
"#;

#[cfg(test)]
mod tests {
    use super::{emit_ranges, tables, COutput};

    #[test]
    fn bootstrap_whitespace_matches_native_unicode_table() {
        let (major, minor, patch) = char::UNICODE_VERSION;
        let mut expected = COutput::new(1024);
        expected.push_str(&format!(
            "/* Rust Unicode whitespace {major}.{minor}.{patch}. Generated from char::is_whitespace(). */\n"
        ));
        emit_ranges(&mut expected, "ku_unicode_space", &tables().whitespace);

        let actual = include_str!("../../bootstrap/tools/compiler_driver/string_trim_whitespace.h");
        assert_eq!(
            actual.replace("\r\n", "\n"),
            expected.finish().expect("whitespace table fits"),
            "regenerate the bootstrap whitespace table with the pinned Rust toolchain"
        );
    }
}
