---
title: '표준 라이브러리'
description: 'Ku 표준 라이브러리 모듈, 주요 시그니처, 백엔드 제한.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
  ]
---

이 문서는 실험적 사전 릴리스 v0.0.18의 확정된 범위를 설명합니다. 실제 공개 상태와 다운로드는 GitHub Releases에서 확인하세요. 주요 진입점을 소스와 대조했으며 모든 API를 다루는 참조 문서는 아닙니다. 동작은 [문법 계약](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md)이 정의합니다. 백엔드 제한은 [지원 표](/ko/docs/support/)를 참고하세요.

## 모듈 가져오기

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes`, `net`은 명시적으로 가져와야 합니다. 일부 기존 모듈은 여전히 직접 호출할 수 있지만 새 예시는 의존성을 명확히 하기 위해 명시적 가져오기를 사용합니다. `std.task`는 내부 관측과 스트레스 테스트용 네임스페이스이며 애플리케이션 동시성 API가 아닙니다.

| 모듈                                 | 목적                                     | 기억할 제한                                                           |
| ------------------------------------ | ---------------------------------------- | --------------------------------------------------------------------- |
| `std.string` / 문자열 메서드         | 유니코드 길이, 바이트 길이, 조회, 자르기 | 네이티브 `trim/lower/upper`는 유니코드 구현이 없음                    |
| `std.array` / 배열 메서드            | 길이, 조회, 새 배열, map                 | 직접 인덱스 실패는 복구 가능한 `try_get`과 다름                       |
| `std.object`                         | `get_or`로 명시적 기본값 조회            | 없는 일반 인덱스는 null을 기본값으로 반환하지 않음                    |
| [std.json](/ko/docs/stdlib/json/)    | 엄격한 JSON 인코딩 및 디코딩             | 네이티브 Task와 LLVM 미지원                                           |
| `std.fs`                             | 파일, 존재 확인, 디렉터리 목록           | 상대 경로는 현재 `.ku` 소스 디렉터리를 기준으로 해석                  |
| `std.time`                           | 시간, 날짜, 기간, 대기                   | 동기 대기와 Task 대기는 다른 실행 경로 사용                           |
| `std.http`                           | HTTP 클라이언트, 서비스, 응답            | 백엔드와 생명주기 차이가 있으며 완전한 범용 Task 네트워크 구현이 아님 |
| `std.config`                         | 환경 및 설정                             | 실제 자격 증명을 소스에 커밋하지 말 것                                |
| `std.pg` / `std.redis` / `std.mysql` | 제한된 풀을 가진 통합 클라이언트         | 네이티브 전용. 실제 서버 검증과 TLS 제한은 별도 확인 필요             |
| `std.bytes` / `std.net`              | 이진 데이터, TCP 클라이언트, 선택적 TLS  | 생성된 네이티브 C 경로. TLS에는 대상에 맞는 팩 필요                   |
| `std.lexer` / `std.parser`           | 컴파일러 디버깅 보조 도구                | 완전한 공개 Token / AST 구조가 아닌 디버그 요약 반환                  |

## 일반적인 시그니처

다음은 설명용 시그니처입니다. `&`는 지원되는 읽기 전용 매개변수 모드, `T!`는 복구 가능한 결과를 나타냅니다:

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()`은 2이고 `.byte_len()`은 7입니다. 길이는 유니코드 스칼라 값을 셉니다. 자르기는 UTF-8 바이트가 아닌 문자 인덱스를 사용합니다. `array.push`와 `array.concat`은 입력을 변경하지 않고 새 배열을 반환합니다.

## 파일 읽기

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

먼저 소스 옆에 UTF-8 `message.txt`를 만드세요. 파일이 없거나 읽기에 실패하면 오류를 반환합니다. 이 예시는 해당 입력 파일에 의존합니다. `try_read`와 `try_write`는 같은 결과 타입을 갖는 호환 별칭입니다.

`read_dir`는 정렬된 독립 소유 경로를 반환하며 최대 10,000개 항목, 총 경로 길이 1,000,000바이트로 제한됩니다. 제한된 네이티브 Task 파일 작업은 고정 백그라운드 풀을 사용합니다. 같은 메커니즘으로 범용 네트워크 대기가 구현되었다는 뜻은 아닙니다.

## 참조 문서 유지 관리

공개 인터페이스 메타데이터는 [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/stdlib/metadata.rs)에 있습니다. 구현과 계약을 함께 대조합니다. 이 웹사이트는 현재 내용을 선별한 수동 참조 문서를 유지합니다. 기존 `ku doc` 명령은 확인되지 않았습니다.

## API reference (English)

Detailed module references and runnable examples are currently available in English:

- [http](/en/docs/stdlib/http/)
- [time](/en/docs/stdlib/time/)
- [fs](/en/docs/stdlib/fs/)
- [config](/en/docs/stdlib/config/)
- [string](/en/docs/stdlib/string/)
- [array](/en/docs/stdlib/array/)
- [object](/en/docs/stdlib/object/)
- [json](/en/docs/stdlib/json/)
- [bytes](/en/docs/stdlib/bytes/)
- [net](/en/docs/stdlib/net/)
- [database](/en/docs/stdlib/database/)
- [compiler](/en/docs/stdlib/compiler/)
