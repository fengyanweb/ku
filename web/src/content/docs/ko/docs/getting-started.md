---
title: '시작하기'
description: 'Hello Ku를 작성하고 소스를 검사하며 인터프리터 실행과 컴파일의 차이를 알아봅니다.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.18.md']
---

이 문서는 실험적 사전 릴리스 v0.0.18의 확정된 범위를 설명합니다. 실제 공개 상태와 다운로드는 GitHub Releases에서 확인하세요. 이 페이지는 v0.0.17에 이미 있는 기본 문법과 CLI 명령을 사용합니다. 먼저 [Ku를 설치](/ko/docs/install/)하고 `ku --version`이 실행되는지 확인하세요.

## 첫 프로그램 작성

저장소의 `examples/hello.ku`와 같은 내용으로 `hello.ku`를 만듭니다:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()`은 진입점입니다. `println`은 텍스트 한 줄을 출력합니다. Ku는 `let`이나 `let mut`를 사용하지 않습니다.

## 검사 및 실행

파일이 있는 디렉터리에서 실행합니다:

```sh
ku check hello.ku
ku run hello.ku
```

`check`는 프로그램을 실행하지 않고 어휘, 문법, 기본 의미 오류를 검사합니다. 실행 시 예상되는 표준 출력은 다음과 같습니다:

```text
Hello Ku
```

Ku가 `PATH`에 없다면 설치된 실행 파일의 전체 경로로 호출하세요. Windows 실행 파일은 `ku.exe`입니다.

## 변수와 함수

다음 기본 문법 조각을 `main` 안에 넣을 수 있습니다:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

첫 대입이 변수를 선언합니다. 명시적 타입은 `name:type = value` 형식입니다. `print`는 자동 줄바꿈 없이 출력하고 `println`은 줄바꿈을 추가합니다.

## 실행 파일 빌드

필요한 빌드 도구는 설치 페이지에 있습니다. 기본 백엔드에서는:

```sh
ku build hello.ku
```

기본 백엔드는 인터프리터 러너를 패키징합니다. 네이티브 C 컴파일에는 별도의 C 컴파일러가 필요합니다:

```sh
ku build --backend c hello.ku
```

기본 출력은 `.ku/build/<profile>/` 아래에 있습니다. CLI가 알려 주는 정확한 경로를 사용하세요. 두 백엔드의 기능 범위는 다릅니다. `ku check` 통과는 네이티브 빌드 성공을 증명하지 않습니다.

참고: `-o` 없는 `ku build --native hello.ku`는 **C 소스만 출력하고 링크하지 않습니다**. 첫 컴파일 프로그램에는 위의 `--backend c` 명령을 사용하세요.

## 프로젝트 생성

패키지 디렉터리가 필요하면 내장 템플릿을 사용합니다:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

패키지 진입점은 `ku.mod`에 설정합니다. `ku create --list`는 사용 가능한 템플릿을 나열합니다. 현재 `ku test`와 `ku fmt` 명령은 없습니다.

[언어 안내](/ko/docs/guide/language/), [문제 해결](/ko/docs/guide/errors/), [백엔드 지원](/ko/docs/support/)으로 이어가세요. 여기에 나온 출력은 예상 예시이며 사용 중인 플랫폼의 설치 검증을 대신하지 않습니다.
