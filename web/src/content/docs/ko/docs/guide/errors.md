---
title: '오류 및 문제 해결'
description: 'Ku 설치, 가져오기, 소유권, JSON, 백엔드 기능 오류를 진단합니다.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
    'docs/v0.0.18-r1.md',
  ]
---

실험적 개발 과정에서 컴파일러 진단은 달라질 수 있습니다. 문제를 보고할 때 CLI 버전, Git 커밋, 백엔드, 전체 명령, 최소 소스를 포함하세요.

## Ku 실행 파일 확인

```sh
ku --version
ku -h
ku check hello.ku
```

`ku`를 찾을 수 없다면 압축을 푼 디렉터리에서 실행 파일을 호출하세요. Windows에서는 `ku.exe`를 사용합니다. 기본 빌드에 Rust나 링크 라이브러리가 없다면 [설치 요구 사항](/ko/docs/install/)을 확인하고 CLI만 복사하지 말고 전체 압축 파일의 배치를 유지하세요.

## 진단 읽기

일반 검사는 파일, 소스 위치, 소스 발췌, 가능한 수정 안내를 표시합니다. 도구 연동에는 다음 명령을 사용할 수 있습니다:

```sh
ku check --json hello.ku
```

출력은 **JSON Lines**이며 한 줄에 진단 하나를 출력합니다. 필드는 `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes`, `helps` 등을 포함합니다. 성공 시에는 출력하지 않습니다. JSON 출력이 없다고 검사가 실행되지 않은 것은 아닙니다.

| 진단    | 흔한 원인                       | 조치                                                     |
| ------- | ------------------------------- | -------------------------------------------------------- |
| `E0105` | `let` 사용                      | 첫 대입으로 변수 선언                                    |
| `E0302` | bool이 아닌 `if` / `while` 조건 | 명시적인 불리언 비교 작성                                |
| `E0601` | 알 수 없는 표준 라이브러리 모듈 | `std.json` 같은 실제 모듈 경로 확인                      |
| `E0605` | 필수 명시적 가져오기 누락       | `import fs from "std.fs"`처럼 모듈 가져오기              |
| `E0901` | 이동된 값 읽기                  | 독립 값을 명시적으로 복제하거나 지원되는 동기 대여 사용  |
| `E0913` | 비동기 매개변수 대여            | 소유 매개변수를 사용하고 원본이 필요하면 호출자에서 복제 |
| `E0918` | 호출 지점에 `&value` 작성       | 일반 인자를 전달. 대여는 시그니처에서 선언               |

이는 등록된 진단의 요약이며 모든 오류 목록이 아닙니다. 현재의 기준 레지스트리는 [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/error.rs)입니다.

## 런타임 오류에는 별도의 계약이 있음

런타임 Error의 `domain/code` 값은 컴파일러의 `E` 코드와 별개입니다. 잘못된 JSON은 `json/parse_error`, 없는 객체 키는 `object/missing_key`를 반환할 수 있습니다. 실패할 수 있는 호출 뒤에 `?`를 쓰거나 `try/catch`에서 필드를 읽으세요.

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

이 학습용 프로그램은 의도적으로 복구 가능한 파싱 실패를 유발합니다. 보고되는 `byte N`은 0부터 시작하는 UTF-8 바이트 오프셋입니다. 중국어나 이모지 주변의 문자 개수가 아닙니다.

## 네이티브 빌드가 거부됨

`ku check` 통과는 프런트엔드 검사가 성공했다는 사실만 뜻합니다. 네이티브 C와 LLVM은 각각 기능 제한이 있습니다. 구문이 명시적으로 거부되면 [지원 표](/ko/docs/support/)를 확인하세요. 시간 초과를 늘리거나 다른 백엔드에서 성공하더라도 지원하지 않는 경로를 검증한 것이 아닙니다.

`-o` 없는 `ku build --native file.ku`는 C만 출력합니다. 실행 파일을 컴파일하고 링크하려면 `ku build --backend c file.ku`를 사용하고 대상 C 도구 모음을 설치하세요.

데이터베이스 오류 `execution_unknown`과 `execution_completed_without_result`는 자동 재실행의 근거가 되어서는 안 됩니다. 요청과 업무 증거를 보존하고 실제 실행 상태부터 확인하세요.

자세한 내용은 [저장소 진단 안내](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/diagnostics.md)를 참고하세요.

JSON 오류의 domain/code는 백엔드 간 같지만 message 전체는 다를 수 있습니다. ku run과 runner에는 json parse failed:가 붙고 네이티브 C에는 없을 수 있습니다. 전체 문구가 아니라 domain/code로 판단하세요.
