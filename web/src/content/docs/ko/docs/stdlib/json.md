---
title: 'JSON 참조'
description: 'Ku JSON 인코딩, 디코딩, KuValue 변환, 오류 코드 및 백엔드 지원.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.19.md',
  ]
---

> v0.0.18 추가 정보는 현재 영어로 제공하며 기존 번역은 유지합니다. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## 가져오기 및 인터페이스

```ku
import json from "std.json"
```

| 인터페이스                   | 반환       | 의미                                             |
| ---------------------------- | ---------- | ------------------------------------------------ |
| `json.stringify(&value:any)` | `str!`     | 읽기 전용으로 새로운 간결한 JSON 텍스트에 인코딩 |
| `json.parse(text:str)`       | `KuValue!` | 독립적으로 소유한 동적 값으로 엄격하게 파싱      |
| `json.try_parse(text:str)`   | `KuValue!` | `parse`의 호환 별칭                              |
| `json.encode(&value:any)`    | `str!`     | 개발 브랜치의 `stringify` 별칭                   |
| `json.decode(text:str)`      | `KuValue!` | 개발 브랜치의 `parse` 별칭                       |

`!`는 Result를 나타냅니다. `?`로 오류를 전파하거나 `try/catch`로 처리합니다. 인코딩은 입력을 소비하거나 변경하지 않습니다.

## 값 인코딩 및 읽기

다음은 완전한 기본 예시입니다. 개발 브랜치의 [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.19/examples/json.ku)에 더 넓은 회귀 검증 예시가 있습니다.

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

객체 키는 결정적인 순서로 정렬됩니다. 예상 출력:

```text
{"count":7,"name":"Ku"}
Ku
8
```

파싱한 `KuValue`를 정수 연산에 직접 사용할 수 없습니다. 먼저 `.as_int()?`나 `.as_str()?`로 변환하세요. 타입이 맞지 않으면 오류를 반환합니다. 없는 객체 키도 자동 기본값 대신 오류를 반환합니다.

## 데이터 및 경계

지원 입력은 문자열, 불리언, 정수, 유한 부동소수점, null, 중첩 배열, 문자열 키 객체, 표현 가능한 구조체입니다. 함수, Task, 리소스 핸들, Result, 열거형, 순환 참조는 인코딩할 수 없습니다.

| 규칙         | 현재 구현                                                              |
| ------------ | ---------------------------------------------------------------------- |
| JSON 문법    | 엄격함. 주석, 후행 쉼표, 뒤의 추가 입력, 잘못된 유니코드 거부          |
| 입력 / 출력  | 각각 1,000,000바이트 제한                                              |
| 깊이 예산    | 루트 깊이는 0. `depth > 32` 거부. 빈 컨테이너 33단계 허용, 34단계 거부 |
| 정수         | 부호 있는 64비트. 범위 초과는 오류이며 float로 자동 변환하지 않음      |
| 부동소수점   | 유한 binary64. 원래 표기, float 태그, 음의 0을 보존하지 않음           |
| 유니코드     | 중국어, 이모지, 이스케이프, 포함된 NUL                                 |
| 중복 객체 키 | 파싱 시 마지막 값 유지                                                 |
| 키 순서      | 인코딩 시 UTF-8 바이트 순서로 정렬                                     |

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## 오류

복구 가능한 오류는 `json` 도메인을 사용합니다. 주요 코드는 `parse_error`, `stringify_error`, `out_of_memory`입니다. 파싱 메시지의 `byte N`은 0부터 시작하는 UTF-8 바이트 오프셋입니다. 최소한의 잘못된 입력 처리기는 [오류 및 문제 해결](/ko/docs/guide/errors/)을 참고하세요.

## 백엔드 지원

인터프리터, 기본 패키징 러너, 동기 네이티브 C 백엔드에 JSON 구현이 있습니다. **제한된 네이티브 Task 부분집합과 LLVM은 JSON을 지원하지 않습니다.** 기본 러너는 가져온 소스의 원래 위치에 여전히 의존합니다. JSON 지원이 완전히 독립적인 패키지를 뜻하지는 않습니다.

이 페이지의 시그니처와 제한은 소스 및 계약과 대조했습니다. 실행 검증에는 버전, 커밋, 백엔드를 명시해야 합니다. 전체 [JSON 문법 계약](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md#127-json)을 참고하세요.
