---
title: '언어 안내'
description: 'Ku의 변수, 함수, 제어 흐름, 소유권, 모듈, 복구 가능한 오류를 배웁니다.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
  ]
---

이 문서는 실험적 사전 릴리스 v0.0.18의 확정된 범위를 설명합니다. 실제 공개 상태와 다운로드는 GitHub Releases에서 확인하세요. 이 안내는 핵심 문법을 소개합니다. 저장소의 [문법 계약](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md)과 [의미 계약](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md)이 기준입니다. 아래는 학습용 코드 조각입니다. 차이점은 [백엔드 지원](/ko/docs/support/)을 참고하세요.

## 변수와 타입

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

첫 대입이 변수를 선언합니다. 기본 타입은 `int`, `float`, `bool`, `str`, `null`입니다. 정수는 부호 있는 64비트이며 부동소수점은 binary64를 사용합니다. 정수를 참으로 간주하는 암시적 변환으로 조건에 사용할 수 없습니다.

## 함수와 제어 흐름

다음 완전한 기본 예시는 `examples/function.ku`에서 가져왔습니다:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

예상 출력은 `30`입니다. 다음 제어 흐름 조각은 `main` 안에 넣을 수 있습니다:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5`는 `0`부터 `4`까지 반복합니다. `if`와 `while` 조건에는 `bool`이 필요합니다. 여러 문장은 중괄호로 묶습니다. 분기 표현식은 `match`를 사용하며 `switch`는 없습니다.

## 소유권과 읽기 전용 대여

`int`, `float`, `bool`, `null`은 Copy 값입니다. 문자열, 배열, 객체, 구조체, 열거형, Result, 함수 값은 Owned입니다. 대입과 일반 인자는 기본적으로 소유 값을 이동합니다. 독립적인 값이 필요하면 `.clone()`을 사용하세요. Task는 복제할 수 없습니다.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

동기 함수는 읽기 전용으로 빌린 매개변수를 선언할 수 있습니다:

```ku
fn length(&text: str): int {
    return text.len()
}
```

인자에 `&`를 붙이지 않고 `length(value)`로 호출합니다. 빌린 데이터는 변경하거나 이동하여 꺼낼 수 없고 호출 범위를 벗어날 수도 없습니다. 비동기 매개변수 대여 같은 조합은 아직 지원하지 않습니다. 읽기 전용 대여가 임의 함수의 순수성을 보장하지는 않습니다.

## 구조체, 객체, 모듈

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

동적 객체는 `{ name: "Ku" }`를 사용합니다. 일반 객체 인덱스에 키가 없으면 실패합니다. `value["name"]?`는 Result를 통해 조회를 처리하고 성공하면 `KuValue`를 반환합니다. 구체 타입이 필요하면 `.as_str()?`나 `.as_int()?`를 사용하세요. 없는 키가 조용히 `null`이 되지는 않습니다.

표준 라이브러리는 `import json from "std.json"`처럼 점으로 구분한 경로를 사용합니다. 로컬 모듈은 `import math from "./math"`를 사용할 수 있습니다. 사용자 정의 최상위 내보내기는 ASCII 대문자로 시작해야 합니다. 소문자로 시작하는 이름은 파일 내부에 한정됩니다.

## 복구 가능한 오류

반환 타입 `T!`는 T 또는 Error를 반환할 수 있습니다. `?`는 복구 가능한 오류를 전파하고 `fail`은 오류를 만듭니다:

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error에는 `domain`, `code`, `message` 필드가 있습니다. 일반 `catch`는 `panic`, Task 취소, 시간 초과를 잡지 않습니다. finally에서의 취소 정리는 같은 절대 기한 예산을 공유합니다. 임의의 사용자 정리 작업 완료는 보장하지 않습니다.

## 동시성의 경계

애플리케이션 코드는 `async fn`을 호출하여 한 번만 사용할 수 있는 Task 핸들을 얻은 뒤 `await`를 사용합니다. `task.spawn`, `Task.new`, `thread.spawn`이라는 애플리케이션 API는 없습니다. 인터프리터와 네이티브 Task 부분집합의 지원 범위는 다릅니다. 한 백엔드에서 동작하는 예시가 다른 백엔드의 완전한 지원을 증명하지는 않습니다. [플랫폼 및 백엔드 지원](/ko/docs/support/)을 참고하세요.
