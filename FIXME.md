# Crash Monitor FIXME

작성일: 2026-07-17

이 문서는 Crash Monitor에 대해 확인된 수정 사항을 중복 없이 통합한 자체 완결형 실행 목록이다. 원문을 열어보지 않아도 구현 범위와 완료 조건을 판단할 수 있도록 각 항목에 필요한 내용을 직접 적었다.

## 사용 원칙

- 같은 원인의 중복 지적은 하나의 FIXME로 합쳤다. 하위 체크 항목까지 모두 끝나야 상위 항목을 완료한 것으로 본다.
- `P0`는 출시 전 차단 항목, `P1`은 높은 운영·정확성·보안 위험, `P2`는 구조·품질·문서 개선이다.
- 후속 검증에서 무효로 확인된 주장은 실행 목록에서 제외하고 마지막의 `구현 시 전제로 삼지 말아야 할 주장`에 기록했다.
- 코드 변경과 함께 관련 단위·통합·E2E 테스트 및 문서를 같은 작업에서 갱신한다.

## P0 — 출시 전 차단 항목

### P0-01. 모든 비정상 종료를 구조화해 보고한다

- [x] `exit(non-zero)`, SIGKILL 이외의 signal 종료, core dump를 각각 `ExitFailure`/`SignalFailure` 같은 typed report로 생성한다.
- [x] exit code, signal, core-dump 여부와 실행 시간을 보존한다.
- [x] 최초 50ms 안에 이미 reap한 상태를 포함해 모든 terminal `WaitStatus`를 하나의 `TerminationReason`으로 전달한다.
- [x] monitor 내부 실패, 감지된 crash, child 자체 종료가 서로 다른 exit-code namespace를 사용하고 원 signal semantics를 잃지 않게 한다.
- [x] 정상 빠른 종료와 exec failure를 구분하는 테스트를 추가한다.

범위: `src/main.rs`, `src/event_loop.rs`, `src/platform/macos/ffi/spawn.rs`, report model.

### P0-02. 동기 capture와 비동기 finalize를 분리한다

- [x] Mach exception critical path를 deadline이 있는 bounded immutable snapshot 생성까지만 허용한다.
- [x] snapshot 직후 task를 resume하고 Mach reply를 보낸다.
- [x] symbolication, JSON/PNG/ZIP, move, retention, feedback, notifier는 worker process 또는 bounded worker queue에서 수행한다.
- [x] feedback·ZIP·notifier hang이 child resume나 Mach reply를 지연하지 않는 테스트를 추가한다.
- [x] snapshot/finalize 각 단계의 deadline과 failure policy를 문서화한다.
- [x] timeout 후 detach될 수 있는 capture worker가 task send right를 독립 소유하게 해 supervisor handle drop 뒤 Mach port name 재사용을 막는다.
- [x] fatal finalizer thread spawn이 연속 실패해도 plugin을 실행하지 않는 동기 emergency transaction으로 Stage-1 raw/SHM과 best-effort JSON을 보존한다.
- [x] cooperative collector의 in-flight Mach 호출까지 resume 전에 종료하거나 kill 가능한 경계로 격리해 resume 이후 task-port 접근 자체를 제거한다.

범위: `src/event_loop.rs`, `src/pipeline`, `src/postprocessors`, `src/notifiers`.

### P0-03. process-global SIGALRM timeout을 안전한 deadline 모델로 교체한다

- [x] `alarm()`/SIGALRM 기반 plugin timeout을 제거한다.
- [x] 비신뢰·blocking plugin은 kill 가능한 별도 process에서 실행하고, 협력적 plugin은 deadline/cancellation token을 사용한다.
- [x] 동시 plugin 실행, CPU-bound loop, EINTR 재시도 I/O, sleep/hung worker를 실제 subprocess로 검증한다.
- [x] timeout 결과를 `TimedOut`으로 진단하고 기존 signal handler를 변경하지 않게 한다.
- [x] 전환 전까지 문서에는 일부 interruptible syscall만 중단할 수 있다는 현재 보장 범위를 정확히 적는다.

범위: `src/pipeline/safety.rs`, timeout tests, `docs/pipeline.md`. 주의: signal이 현재 Mach receive를 `MACH_RCV_INTERRUPTED`로 끝낸다는 인과는 무효다.

### P0-04. `enabled=false`를 실제 kill switch로 만든다

- [x] validated config와 `Pipeline`에 명시적 global enabled 상태를 둔다.
- [x] disabled이면 `handle_event` 입구에서 suspend, collector, raw/JSON write, postprocessor를 실행하지 않는다.
- [x] crash, exit, signal, probable OOM, ANR, snapshot trigger별 enable 의미를 분리한다.
- [x] 항상 남길 emergency evidence가 필요하다면 global disable의 예외가 아니라 별도 명시 정책으로 정의한다.
- [x] disabled 상태에서 합의된 동작 외에는 파일이 생성되지 않는 테스트를 추가한다.

범위: `src/config.rs`, `src/main.rs`, `src/pipeline/mod.rs`.

### P0-05. 모든 plugin 설정 조합을 panic 없이 검증한다

- [x] config load 시 plugin dependency closure를 계산한다.
- [x] dependency를 자동 활성화할지 dependent를 자동 비활성화할지 정책을 정하고 진단한다.
- [x] collector/preprocessor/filter/postprocessor/notifier의 hard dependency와 order-only dependency를 구분한다.
- [x] startup `panic!` 대신 구조화된 `ConfigValidationError`를 반환한다.
- [x] toggle 조합, duplicate plugin ID, cycle, missing dependency를 property test로 검증한다.

범위: `src/config.rs`, `src/pipeline/mod.rs`, `src/pipeline/types.rs`.

### P0-06. task suspend/resume 소유권을 RAII로 보장한다

- [x] 성공한 suspend에서만 생성되는 `TaskSuspendGuard`를 도입한다.
- [x] explicit finish와 `Drop` 어느 경로에서도 resume가 정확히 한 번 실행되게 한다.
- [x] suspend 실패 시 소유하지 않은 suspension count를 변경하지 않고 capture 정책을 명시한다.
- [x] resume 실패를 diagnostics와 supervisor health에 기록하고 bounded retry 후 terminate/escalate 정책을 적용한다.
- [x] panic, early return, plugin failure, timeout별 suspend/resume 균형 테스트를 추가한다.

범위: `src/pipeline/mod.rs`, platform task API.

### P0-07. live SHM에서 빌린 참조를 제거하고 suspend 중 owned snapshot을 만든다

- [x] 다른 process가 변경할 수 있는 mapping 위에 일반 Rust reference나 safe borrowed slice를 만들지 않는다.
- [x] raw pointer에서 bounded owned bytes/typed snapshot으로 복사한다.
- [x] 모든 section과 Stage 1 SHM dump를 child resume 전에 snapshot한다.
- [x] resume 후에는 해당 이벤트의 payload section을 다시 읽지 않는다. watchdog heartbeat처럼 계속 관측해야 하는 live state는 별도 atomic API로만 읽는다.
- [x] generation counter 또는 seqlock으로 torn snapshot을 탐지한다.

범위: `src/shm/reader.rs`, `src/pipeline/mod.rs`, `src/pipeline/safety.rs`.

### P0-08. SHM release/acquire와 일관성 계약을 구현한다

- [x] C producer의 release store와 Rust consumer의 정렬된 `AtomicU32`/`AtomicU64` acquire load를 실제 ABI로 구현한다.
- [x] heartbeat, valid flag, ring state, payload publication 순서를 하나의 명시적 계약으로 정한다.
- [x] C/Rust 양쪽에 atomic alignment, size, 핵심 offset compile-time assertion을 둔다.
- [x] multi-field snapshot에는 generation/seqlock을 적용한다.
- [x] concurrent producer/consumer stress test와 torn-write fixture를 추가한다.
- [x] `docs/shared-memory.md`의 acquire 설명과 volatile 설명 사이 모순을 제거한다.

범위: `schema/crash_shm.h`, `src/shm/reader.rs`, `docs/shared-memory.md`.

### P0-09. 신뢰하지 못하는 SHM 값을 Rust validity invariant에 맞게 검증한다

- [x] C `bool`인 `git_dirty`를 wire schema에서 `uint8_t`로 바꾸고 Rust에서 명시적으로 변환한다.
- [x] untrusted `git_dirty` byte를 Rust `bool`로 구체화하지 않고 먼저 integer로 읽는다.
- [x] `git_dirty`의 wire contract를 `0=false`, non-zero=`true`로 정의하고 0, 1, 2, 255를 테스트한다.
- [x] category/severity 같은 wire integer는 복사 후 semantic range를 검증하고, C char array는 bounded NUL/string/control-character 규칙으로 별도 처리한다.
- [x] 범위 밖 semantic value와 손상된 문자열을 포함한 malformed SHM 테스트를 추가한다.
- [x] schema 변경에 맞춰 version을 올리고 producer compatibility 정책을 문서화한다.

범위: `schema/crash_shm.h`, `src/shm/reader.rs`.

### P0-10. producer readiness와 monitor 시간을 ANR에서 분리한다

- [x] `producer_ready` handshake 또는 명시적인 ANR opt-in을 도입한다.
- [x] 최초 heartbeat publication 전에는 watchdog을 시작하지 않는다.
- [x] monitor가 child를 suspend하거나 pipeline을 처리한 시간은 hang duration에서 제외한다.
- [x] event 처리 후 heartbeat 기준 시각/accumulator를 안전하게 재설정한다.
- [x] SHM 미연동 장기 실행 child와 느린 snapshot 직후의 false ANR 회귀 테스트를 추가한다.

범위: `src/main.rs`, `src/event_loop.rs`, `src/watchdog.rs`.

### P0-11. Mach exception message를 실제 MIG wire layout으로 파싱한다

- [x] 정렬 1 byte buffer를 FFI struct reference로 cast하지 않는다.
- [x] 커널 MIG의 4-byte packing을 따르는 byte-offset parser, generated binding 또는 C shim을 사용한다.
- [x] 실제 received `msgh_size`, expected `msgh_id`, descriptor count/type/disposition, code count를 엄격히 검증한다.
- [x] raw code/subcode를 정확한 offset에서 읽고 원본 배열도 report에 보존한다.
- [x] parse 실패 또는 header mismatch에서도 가능한 request에는 `KERN_FAILURE` reply를 보내 faulting thread가 영구 대기하지 않게 한다.
- [x] listener fatal failure와 channel disconnect를 supervisor에 전달한다.
- [x] 실제 kernel fixture와 malformed/truncated message corpus로 alignment, pack(4), reply behavior를 테스트한다.

범위: `src/platform/macos/exceptions.rs`, `src/platform/macos/ffi/exceptions.rs`.

### P0-12. VM region 열거에 진행·시도·시간 상한을 둔다

- [x] 성공 region 수와 별개로 total attempt와 consecutive error cap을 둔다.
- [x] 매 반복에서 address가 단조 증가하는지 검사하고 saturating 정지 상태를 종료한다.
- [x] 고정 4KiB 대신 실제 host page size를 사용한다.
- [x] 전체 capture deadline을 적용하고 partial 결과의 품질을 진단한다.
- [x] 연속 오류, address overflow, no-progress, 매우 큰 map 테스트를 추가한다.

범위: `src/platform/macos/memory.rs`, `src/platform/macos/ffi/memory.rs`, `src/event_loop.rs`.

### P0-13. event-scoped `ReportId`와 원자적 artifact transaction을 도입한다

- [x] event 생성 시 UUID 또는 boot nonce+monotonic sequence 기반 `ReportId`를 한 번만 만든다.
- [x] 모든 stage, attachment, raw, screenshot, JSON, ZIP, notifier가 같은 immutable `ReportContext`를 사용한다.
- [x] report별 전용 directory와 exact manifest를 사용하고 basename prefix 탐색을 제거한다.
- [x] temp write, fsync, atomic rename으로 manifest를 마지막에 commit한다.
- [x] 같은 PID·type·second에 두 event가 생겨도 정확히 두 report가 생성되는 테스트를 추가한다.
- [x] 처리 도중 강제 종료 후 재시작해도 partial artifact가 노출되지 않거나 scavenger가 복구하게 한다.

범위: `src/pipeline`, collectors, postprocessors, event-loop tests.

### P0-14. report 저장을 private-by-default로 강제한다

- [x] data/report directory를 명시적으로 `0700`, JSON/raw/RGBA/PNG/ZIP/temp를 `0600`으로 생성한다.
- [x] 기존 directory의 owner, mode, symlink/ACL 정책을 검증하고 안전하지 않으면 실패하거나 교정한다.
- [x] create에는 `O_EXCL`/`O_NOFOLLOW`를 적용하고 final path는 atomic rename으로 publish한다.
- [x] environment, memory, screenshot, attachment의 기본 수집을 최소화하고 privacy level/consent/retention/encryption 정책을 정의한다.
- [x] 일반 umask와 제한적 umask 모두에서 최종 mode를 검증한다.

범위: `src/utils/paths.rs`, artifact writers, config/docs. 주의: 현재 mode가 항상 0755/0644라는 단정이 아니라 private mode를 강제하지 않는 것이 문제다.

## P1 — 프로세스 수명주기와 event 처리

### P1-01. task port 획득 실패를 bounded policy로 처리한다

- [x] `task_for_pid` 재시도 종료 후 무기한 blocking `waitpid`로 들어가지 않는다.
- [x] 설정된 deadline 뒤 child를 terminate/reap할지, 감시 불가 상태로 detach할지 명시적으로 선택한다.
- [x] entitlement 부재, 장기 실행 child, child 조기 종료를 각각 테스트한다.

범위: `src/main.rs`, task-port acquisition.

### P1-02. monitor가 child process group의 전체 수명주기를 소유한다

- [x] spawn 시 별도 process group을 만들고 PID, PGID, task port, exception port, listener, SHM을 supervisor가 소유한다.
- [x] SIGTERM/SIGINT를 self-pipe/event source로 받아 정책에 따라 child group에 전달한다.
- [x] monitor 정상 종료, 내부 오류, 강제 종료 가능한 범위에서 child terminate/reap과 SHM/port 정리를 보장한다.
- [x] parent-death guard 또는 fixture 자체 deadline으로 monitor 비정상 종료 시 orphan 위험을 줄인다.

범위: `src/main.rs`, `src/event_loop.rs`, `src/platform/macos/ffi/spawn.rs`.

### P1-03. exception 구독 범위와 종료 의미를 명시한다

- [x] `EXC_BREAKPOINT`, `EXC_RESOURCE`, `EXC_GUARD`를 capture할지 ignore할지 정책을 정한다.
- [x] 구독하는 각 exception type에 report type, severity, signal mapping, raw-code 보존 규칙을 둔다.
- [x] fatal crash 후 monitor가 무조건 1을 반환하지 않고 원 child signal/termination 의미를 보존한다.

범위: `src/platform/macos/types.rs`, `src/event_loop.rs`, report schema.

### P1-04. 50ms polling을 event-driven wait로 교체한다

- [x] process exit, exception wakeup, signal pipe, watchdog timer를 `kqueue` 또는 동등한 deadline-aware wait에 통합한다.
- [x] wakeup 지연, idle CPU 사용량, signal burst, simultaneous exit/exception을 검증한다.

범위: `src/event_loop.rs`, `src/event_source.rs`.

### P1-05. signal self-pipe를 async-signal-safe하게 관리한다

- [x] pipe 양 끝에 `CLOEXEC`를 설정하고 write end를 nonblocking으로 만든다.
- [x] handler 진입 시 `errno`를 저장하고 반환 전에 복원한다.
- [x] pipe full은 안전하게 coalesce하고 handler가 block하지 않게 한다.
- [x] drain은 `EAGAIN`까지 반복하고 `EINTR`는 재시도하며 다른 오류는 전파한다.
- [x] `OnceLock` 초기화 실패를 무시하지 않고 닫힌/reused FD가 publish되지 않게 한다.

범위: `src/event_source.rs`, spawn file-descriptor policy.

### P1-06. exception listener 상실을 health failure로 처리한다

- [x] exception channel의 `Empty`와 `Disconnected`를 구분한다.
- [x] 실제 receive failure로 listener가 종료되면 supervisor에 원인을 전달한다.
- [x] listener 재시작 또는 monitor의 명시적 오류 종료 중 하나를 정책으로 정한다.
- [x] crash 감지 능력을 잃은 채 child를 계속 실행하지 않는다.

범위: `src/event_source.rs`, exception listener.

### P1-07. child environment 전달을 결정적이고 byte-safe하게 만든다

- [x] `std::env::vars()` 대신 non-UTF-8을 보존하거나 안전하게 건너뛸 수 있는 `vars_os()` 기반 경로를 사용한다.
- [x] 상속된 `CRASH_MONITOR_SHM`을 제거한 뒤 새 값 하나만 넣는다.
- [x] SHM 생성 실패 시 `CRASH_MONITOR_SHM=1`을 보내지 말고 key를 생략하거나 명시적 disabled protocol을 사용한다.
- [x] child에 전달한 최종 environment snapshot을 report collector에 주입한다.

범위: `src/main.rs`, `src/collectors/environment.rs`.

### P1-08. child stdout/stderr의 bounded tail을 보존한다

- [x] spawn file actions로 stdout/stderr를 tee하거나 bounded ring buffer에 수집한다.
- [x] child가 대량 출력해도 backpressure로 교착하지 않으며 최대 byte 수를 넘지 않게 한다.
- [x] 종료 report에 stream별 tail, truncation 여부, read 오류를 포함한다.

범위: `src/platform/macos/ffi/spawn.rs`, termination report.

### P1-09. entitlement 검사를 실제 값으로 수행한다

- [x] 파일 문자열 `contains` 검사를 제거하고 signed entitlement plist의 boolean `true`를 파싱한다.
- [x] 가능하면 Security.framework 기반 검증을 사용한다.
- [x] key 없음, `false`, malformed signature, unsigned binary를 구분해 안내한다.

범위: `src/main.rs`, signing checks.

### P1-10. 지원 architecture를 코드와 배포 계약에 일치시킨다

- [x] ARM64 전용이라면 x86_64/Rosetta에서 compile-time 또는 startup 오류를 명확히 낸다.
- [x] 범용 지원이 필요하면 architecture별 thread flavor, register layout, Mach-O slice 처리를 분리한다.
- [x] 최소 macOS와 architecture 조합을 CI와 배포 문서에 명시한다.

범위: `src/platform/macos/types.rs`, collectors, packaging docs.

### P1-11. `waitpid`와 child-state edge case를 손실 없이 처리한다

- [x] `ChildGone`/`ECHILD`를 exit 0과 구분해 `UnknownStatus` 또는 monitor error로 표현한다.
- [x] `StillAlive | _` 같은 wildcard를 제거하고 모든 terminal/stopped/continued 상태를 exhaustive match한다.
- [x] `EINTR`를 재시도하고 unexpected wait 오류를 살아 있는 child의 종료로 오판하지 않는다.
- [x] non-crash nonzero exit가 crash 전용 port destroy/지연 경로를 거치지 않게 한다.

범위: `src/event_loop.rs`, `src/event_source.rs`, `src/platform/macos/ffi/spawn.rs`, `src/main.rs`.

### P1-12. spawn signal state와 오류 보고를 명시한다

- [x] `POSIX_SPAWN_SETSIGMASK`로 child signal mask를 명시적으로 설정한다.
- [x] 정책상 필요한 ignored signals만 `POSIX_SPAWN_SETSIGDEF`로 복구한다.
- [x] `posix_spawn` 계열 return code를 `io::Error::from_raw_os_error`로 해석해 원인을 표시한다.

범위: `src/platform/macos/ffi/spawn.rs`.

### P1-13. event-loop API의 상태 묶음과 이름을 정리한다

- [x] 다수의 위치 인자를 `EventLoopContext` 같은 구조체로 묶어 task와 PID 같은 동일 타입 인자를 뒤바꿀 위험을 없앤다.
- [x] watchdog의 `check_interval_ms`처럼 실제로 elapsed time을 받는 인자를 `elapsed_ms`로 고친다.
- [x] 잘못된 인자 조합을 만들 수 없는 typed handle을 사용한다.

범위: `src/event_loop.rs`, `src/watchdog.rs`.

## P1 — SHM·FFI 계약

### P1-14. 전체 SHM layout을 하나의 schema에서 생성한다

- [x] 64-byte header, magic/version/canary, attachment section, section 순서와 offset을 `schema/crash_shm.h`의 single source of truth로 옮긴다.
- [x] screenshot slot 수의 literal `96`, attachment slot 수의 literal `4`를 제거하고 generated constant/array length를 사용한다.
- [x] section size를 `size_of`/`offset_of`에서 유도하고 generated screenshot struct와 직접 대조한다.
- [x] screenshot reader test의 `SECTION4_OFFSET + 96*4` timestamp 계산을 `offset_of!(SutScreenshotSection, timestamp)`로 바꾼다.
- [x] C producer와 Rust consumer 양쪽에 모든 핵심 struct size, field offset, section alignment assertion을 추가한다.
- [x] layout 변경 시 schema version을 올리는 규칙과 호환 정책을 문서화한다.

범위: `schema/crash_shm.h`, `src/shm/types.rs`, `tests/unit/shm/reader_tests.rs`, `tests/e2e/fixtures/crash_app.c`.

### P1-15. SHM mapping ownership의 의존 방향을 단방향으로 만든다

- [x] low-level mapping handle/syscall과 high-level `SharedMemory` reader를 분리한다.
- [x] `shm`이 platform FFI를 사용하면서 platform FFI가 다시 `SharedMemory`의 `Drop`을 구현하는 순환을 제거한다.
- [x] mapping close/unlink 소유자와 실패 시 정리 순서를 타입으로 표현한다.

범위: `src/shm/reader.rs`, `src/platform/macos/ffi/shm.rs`.

### P1-16. SHM 생성과 초기화를 안전하고 지연 할당 친화적으로 만든다

- [x] PID 기반 이름 대신 random nonce를 포함한 이름과 `O_CREAT|O_EXCL`을 사용하고 `EEXIST`는 새 nonce로 재시도한다.
- [x] unlink와 open 사이 TOCTOU를 줄이고 open 뒤 owner, type, size를 `fstat`로 검증한다.
- [x] ftruncate/mmap 등 모든 중간 실패에서 fd close와 `shm_unlink`가 실행되는 create guard를 도입한다.
- [x] 새 약 50MB mapping 전체를 `write_bytes(0)`하지 않고 필요한 header/state/canary만 초기화한다.

범위: `src/platform/macos/ffi/shm.rs`.

### P1-17. SHM validation과 metadata 소비를 완전하게 만든다

- [x] validation의 bool 결과를 magic/version/canary/size/alignment를 구분하는 `ShmValidationError`로 바꾼다.
- [x] screenshot `tier`를 raw/report metadata와 selection policy에 반영한다.
- [x] 중복되고 미사용인 header `ring_count`를 제거·예약 처리하거나 authoritative count로 사용한다.
- [x] invalid header를 조용히 무시하지 말고 bounded diagnostic을 남긴다.

범위: `src/shm/reader.rs`, `src/shm/types.rs`, diagnostics.

### P1-18. task/thread info FFI를 typed aligned API로 바꾼다

- [x] alignment 1 byte wrapper 대신 정렬된 `[u32; N]` 또는 flavor별 전용 타입을 사용한다.
- [x] caller buffer 크기에 맞는 input count를 넘기고 kernel이 실제 반환한 word count를 검증·반환한다.
- [x] ARM64 thread state는 ABI의 68-word 전용 타입을 우선 사용한다. 가변 입력을 받을 때 현재 consumer의 최대 index 66을 기준으로 최소 67 words를 검사한다.
- [x] 작은 buffer, short count, unknown flavor를 테스트한다.

범위: `src/platform/mod.rs`, `src/platform/macos/ffi/memory.rs`, thread collector.

### P1-19. Mach right 종류별 RAII와 exception-port 정리를 구현한다

- [x] detach될 수 있는 capture worker가 supervisor의 raw task name을 빌리지 않고 독립 send-right user reference를 소유·해제하게 한다.
- [x] receive right에는 send-right용 deallocate가 아니라 `mach_port_mod_refs` 또는 `mach_port_destruct`를 사용한다.
- [x] send, receive, task, thread right를 서로 다른 wrapper로 표현한다.
- [x] exception request descriptor의 task/thread right ownership과 reply 이후 정리 책임을 검증한다.

범위: `src/platform/macos/ffi/exceptions.rs`, Mach port wrappers. 주의: `mach_vm_region.object_name`이 실질 port leak을 만든다는 주장은 이 작업의 근거로 사용하지 않는다.

### P1-20. memory FFI의 short-read 계약을 엄격히 한다

- [x] `vm_read`가 성공을 반환해도 returned byte count가 요청과 다르면 partial result로 처리한다.
- [x] empty/short data를 완전 성공으로 넘기지 않는다.
- [x] caller가 partial diagnostic과 retry/abort 정책을 선택할 수 있게 한다.

범위: `src/platform/macos/ffi/memory.rs`, memory/thread/dylib collectors.

### P1-21. FFI 경계 문서와 mock contract를 실제 구현에 맞춘다

- [x] FFI module allowlist, 경계 테스트 주석, assertion message를 일치시킨다.
- [x] mock의 unknown thread, task-info buffer, thread name 오류 의미를 macOS 구현과 맞춘다.
- [x] mock VM region query는 insertion order가 아니라 가장 낮은 적합 주소를 선택하고 checked arithmetic을 사용한다.

범위: `src/platform/macos/ffi/mod.rs`, `src/platform/mock/mod.rs`.

## P1 — pipeline과 artifact lifecycle

### P1-22. 최소 evidence와 비싼 artifact 작업의 순서를 바로잡는다

- [x] event reason, PID, timestamp, bounded immutable SHM/capture metadata로 구성된 최소 emergency snapshot을 정의하고 filter/duplicate/collector보다 먼저 확보한다.
- [x] collector가 채운 thread data에 의존하는 기존 `write_raw_stage1`은 collector 뒤, preprocessor와 duplicate short-circuit보다 앞에서 실행한다.
- [x] filter/rate-limit 또는 duplicate 판정이 앞서 확보한 최소 emergency snapshot까지 제거하지 않게 한다.
- [x] attachment는 metadata만 먼저 수집하고 report commit이 결정된 뒤 복사한다.
- [x] duplicate, filter rejection, plugin failure, early return에서 이미 만든 임시 artifact를 정리한다.

범위: `src/pipeline/mod.rs`, `src/pipeline/safety.rs`, `src/collectors/attachment.rs`.

### P1-23. plugin 실행 결과를 typed diagnostics로 보존한다

- [x] `Ok`, `Partial`, `Rejected`, `Error`, `Panic`, `Timeout`, `Skipped`를 서로 다른 상태로 모델링한다.
- [x] collector가 내부 오류를 로그만 남기고 빈 성공으로 바꾸지 않게 한다.
- [x] `DylibCollector`의 image 열거 실패를 `unwrap_or_else(... vec![])` 뒤 `Ok(())`로 바꾸지 않고 `Partial` 또는 `Error`와 원인으로 전파한다.
- [x] filter의 passed/rejected/error와 차단한 plugin 이름을 기록한다.
- [x] artifact가 없어 notifier를 건너뛴 경우에도 `Skipped` 사유를 기록한다.
- [x] panic payload와 실제 error chain을 단일 `failed or panicked` 문자열로 축약하지 않는다.

범위: pipeline traits/safety/diagnostics와 각 collector.

### P1-24. postprocessor와 notifier 결과까지 최종 진단에 포함한다

- [x] Stage 2 JSON 작성 뒤 발생하는 ZIP, move, retention, feedback, notifier 상태와 총 시간을 별도 final manifest 또는 원자적 final update로 보존한다.
- [x] diagnostics를 기록하기 위해 notifier가 삭제된 중간 JSON path에 의존하지 않게 한다.
- [x] 각 stage의 시작·종료·실패·duration을 report identity와 함께 남긴다.

범위: `src/pipeline/mod.rs`, report/manifest model.

### P1-25. JSON·screenshot·PNG 갱신을 원자적으로 commit한다

- [x] 최초 JSON과 RGBA/screenshot도 final path에 직접 쓰지 않고 private temp file에 기록한다.
- [x] write·flush·fsync 성공 뒤 atomic rename한다.
- [x] PNG 변환은 새 PNG와 갱신 JSON을 모두 성공적으로 commit한 뒤에만 RGBA를 삭제한다.
- [x] disk full, permission denied, process kill, rename 실패에서 JSON과 image reference가 서로 어긋나지 않는 fault-injection test를 추가한다.

범위: `src/pipeline/report.rs`, `src/postprocessors/png_converter.rs`.

### P1-26. ZIP과 이동을 manifest 기반으로 수행한다

- [x] `starts_with(stem)` 파일 검색을 제거하고 현재 `ReportId` manifest의 exact artifact만 archive한다.
- [x] PID `123`/`1234` 또는 prefix가 비슷한 report의 artifact가 섞이거나 삭제되지 않게 한다.
- [x] ZIP 성공과 원본 삭제 뒤 `ReportResult`/manifest가 실제 ZIP 또는 `sent` 최종 경로를 가리키게 한다.
- [x] notifier는 삭제된 JSON이 아니라 최종 artifact descriptor를 받는다.

범위: `src/postprocessors/zip_archiver.rs`, `src/postprocessors/move_to_sent.rs`, notifier traits.

### P1-27. report와 ZIP 입력에 streaming 자원 제한을 적용한다

- [x] plain JSON을 metadata 확인 후 다시 전체 read하는 TOCTOU를 제거하고 open fd에서 최대 크기+1까지만 읽는다.
- [x] ZIP entry의 선언 `size()`가 아니라 실제 decompressed stream byte 수를 제한한다.
- [x] archive 전체 bytes, entry count, 개별 entry, compression ratio, nesting 정책을 설정한다.
- [x] ZIP write는 `fs::read` 대신 streaming copy를 사용해 peak memory를 단일 buffer로 제한한다.
- [x] symlink를 `symlink_metadata`와 `O_NOFOLLOW`로 거부한다.

범위: `src/pipeline/report.rs`, `src/postprocessors/zip_archiver.rs`.

### P1-28. raw artifact를 manifest로 추적하고 정제 정책을 적용한다

- [x] thread raw뿐 아니라 SHM, breadcrumb, context 등 모든 raw artifact를 manifest에 등록한다.
- [x] sanitizer 이전의 민감 raw가 최종 ZIP 또는 `pending`에 의도치 않게 남지 않게 한다.
- [x] 보존이 필요한 raw는 별도 privacy policy, permission, retention 아래 둔다.
- [x] 삭제 성공 시 `ReportResult.raw_path`와 관련 목록에서 제거한다.
- [x] text Stage 1에는 `.txt`/`.jsonl`을 사용하거나 실제 binary framing을 도입하고, 진짜 SHM binary dump의 `.bin`은 유지한다.

범위: `src/pipeline/safety.rs`, `src/postprocessors/raw_cleanup.rs`, manifest.

### P1-29. retention을 logical report 단위로 적용한다

- [x] 개별 file 수가 아니라 report directory/manifest 단위로 count, size, delete한다.
- [x] JSON만 지워지거나 attachment만 남는 부분 삭제를 막는다.
- [x] `max_reports=0`의 의미를 disabled로 명시하거나 config validation에서 거부한다.
- [x] `pending` 실패 잔여물에도 age/size 기반 보존 정책을 적용한다.

범위: `src/postprocessors/retention.rs`, config/docs.

### P1-30. 하나의 resolved output root를 모든 component에 주입한다

- [ ] `report_dir` 설정을 실제 저장 경로에 연결하거나 옵션을 제거한다.
- [ ] pipeline override, disk-space filter, attachment collector, raw writer, move-to-sent, retention, session recorder가 같은 root를 사용한다.
- [ ] 테스트 override와 production factory가 서로 다른 경로를 조용히 사용하지 않게 한다.

범위: `src/config.rs`, pipeline factory, filters/collectors/postprocessors.

### P1-31. session 기록과 log rotation을 다중 instance에 안전하게 만든다

- [ ] `sessions.jsonl`, rotation temp, `session.lock`에 process-safe locking 또는 monitor별 namespace를 사용한다.
- [ ] 한 instance가 다른 살아 있는 instance의 lock을 삭제하지 않게 owner/token을 확인한다.
- [ ] append와 rotation 사이 데이터 유실을 막고 temp filename을 unique하게 만든다.
- [ ] log를 통째로 메모리에 읽지 않고, 손상 line 하나 때문에 이후 정상 tail을 버리지 않는 byte-safe recovery를 구현한다.
- [ ] read 결과가 비정상일 때 빈/절단 파일로 원본을 교체하지 않는다.

범위: `src/postprocessors/session_recorder.rs`, `src/postprocessors/log_rotator.rs`.

### P1-32. rate limit과 duplicate 정책을 event 수명주기에 맞춘다

- [ ] monitor 재시작 뒤에도 crash-loop 보호가 필요하면 bounded persistent state를 사용한다.
- [ ] duplicate key에 report type, severity, process/build identity를 포함해 snapshot/ANR이 fatal crash를 억제하지 않게 한다.
- [ ] usable frame이 없을 때 상수 empty fingerprint를 만들지 않거나 안전한 fallback을 사용한다.
- [ ] 같은 monitor에서 반복 가능한 snapshot/ANR의 duplicate가 timestamp를 계속 갱신해 suppression window를 무한 연장하지 않게 한다.
- [ ] occurrence count와 마지막 관측 시각을 정책에 맞게 분리한다.

범위: `src/filters/rate_limiter.rs`, `src/preprocessors/fingerprint.rs`, `src/preprocessors/duplicate.rs`.

### P1-33. startup recovery/scavenger를 구현한다

- [ ] startup에서 orphan temp, incomplete manifest, stale `pending`, raw, stale SHM을 안전하게 식별한다.
- [ ] commit marker가 없는 artifact는 복구·quarantine·삭제 중 명시된 정책으로 처리한다.
- [ ] 다른 실행 중 instance의 파일을 건드리지 않게 lock/owner/age를 확인한다.
- [ ] pipeline 각 단계에서 강제 종료한 뒤 재시작하는 fault-injection test를 추가한다.

범위: artifact store startup, SHM lifecycle.

### P1-34. report loader에 version gate와 호환 정책을 둔다

- [ ] `header.version`을 모든 loader와 CLI가 검증한다.
- [ ] 지원하지 않는 future/legacy version을 구조화된 오류로 거부하거나 명시적 migration을 수행한다.
- [ ] 핵심 field의 `serde_json::Value` 중심 모델을 typed, versioned struct로 교체한다.
- [ ] version별 compatibility/migration fixture를 유지한다.
- [ ] 문서의 JSON shape가 실제 serializer와 동일한지 CI에서 확인한다.

범위: `src/pipeline/report.rs`, CLI, feedback/PNG consumers, `docs/reports.md`.

### P1-35. plugin framework의 죽은·모호한 API를 정리한다

- [ ] `priority()`를 실제 안정 정렬에 사용하거나 API에서 제거한다.
- [ ] timeout 없음은 `u32::MAX` sentinel 대신 `Option<Duration>` 또는 typed enum으로 표현한다.
- [ ] plugin 이름/ID의 전역 유일성을 builder에서 강제한다.
- [ ] soft order validator는 실제 warning을 내거나 `Result` 기반 hard validation으로 바꾼다.
- [ ] `ReportResult` 이중 binding 같은 불필요한 mutable shadowing을 제거한다.

범위: `src/pipeline/traits.rs`, `src/pipeline/types.rs`, `src/pipeline/mod.rs`.

### P1-36. panic isolation의 build 전제를 강제한다

- [ ] `catch_unwind`를 쓰는 target crate에서 `panic="unwind"`를 compile-time에 검증한다.
- [ ] 상위 workspace profile이나 `RUSTFLAGS`가 `panic=abort`로 바꿔도 잘못된 안전 보장을 제공하지 않게 한다.
- [ ] host `build.rs` 환경만 검사하는 방식에 의존하지 않는다.
- [ ] panic payload가 report diagnostics에 보존되는 테스트를 추가한다.

범위: `Cargo.toml`, target crate root, `src/pipeline/safety.rs`.

## P1 — 진단 정확도, privacy, 자원 상한

### P1-37. SIGKILL과 OOM을 확정적으로 동일시하지 않는다

- [x] SIGKILL만으로 OOM을 확정하지 않고 `PossibleOom`, `UnknownSigkill`처럼 증거 수준을 표현한다.
- [x] CLI의 “OOM으로 종료됨” 확정 문구를 증거에 맞게 바꾼다.
- [x] jetsam/OS pressure 등 추가 근거가 있을 때만 confidence를 높인다.
- [x] OOM trigger의 실제 기본값과 “opt-in” 문서/설정을 일치시킨다.

범위: `src/config.rs`, `src/event_loop.rs`, `src/cli/analyze.rs`, docs.

### P1-38. exception type별로 raw code와 signal을 정확히 해석한다

- [x] 모든 exception의 code를 무조건 `kern_return_t`, subcode를 fault address로 해석하지 않는다.
- [x] raw code array와 numeric unknown value는 항상 보존하고 display name은 별도 field로 둔다.
- [x] 주요 `kern_return` 이름 매핑을 확장한다.
- [x] `EXC_BAD_ACCESS`에서 SIGSEGV와 SIGBUS를 구분한다.
- [x] `EXC_CRASH` code에서 원 signal을 해독하거나 근사임을 명시한다.
- [x] 구독하지 않는 exception type의 가상 영향은 문서에 사실처럼 적지 않는다.

범위: `src/platform/macos/types.rs`, `src/pipeline/report.rs`.

### P1-39. 안정적인 thread identity와 안전한 register/unwind를 구현한다

- [x] monitor-local Mach port name 대신 `THREAD_IDENTIFIER_INFO`의 stable thread ID를 수집한다.
- [x] register state 길이를 검증한 뒤 fixed index를 사용한다.
- [x] ARM64 ABI 결과는 68 words로 받고, 가변 data를 방어적으로 처리할 때 최대 접근 index 66에 필요한 최소 67 words를 확인한다.
- [x] frame pointer와 `+8` 등 모든 address arithmetic에 `checked_add`를 사용한다.
- [x] short memory read는 partial stack으로 진단하고 unchecked frame을 만들지 않는다.
- [x] frame-pointer walk 외 compact unwind 등 fallback을 검토하고 arm64e PAC를 처리한다.
- [x] unwind 품질과 truncation reason을 report에 표시한다.

범위: `src/collectors/thread.rs`, platform thread API.

### P1-40. memory report field를 실제 의미와 일치시킨다

- [x] allocator 사용량이 아닌 resident pages 근사값을 `in_use_bytes`라고 부르지 않는다.
- [x] allocation count가 아닌 VM region 수를 `in_use_count`라고 부르지 않는다.
- [x] `phys_footprint`, internal, compressed 등 이미 수집한 VM summary를 report에 포함한다.
- [x] schema rename과 backward compatibility/migration을 함께 처리한다.

범위: `src/collectors/memory.rs`, `src/preprocessors/report_formatter.rs`, report schema.

### P1-41. dylib identity와 실제 image range를 수집한다

- [x] 각 image의 LC_UUID, architecture, slide, 실제 segment range를 수집한다.
- [x] “가장 가까운 낮은 base”만으로 주소를 image에 귀속하지 않고 `__TEXT` 범위 안인지 확인한다.
- [x] 임의의 image별 256MB window를 제거한다.
- [x] NUL 없는 C string을 여러 번 읽다가 후속 read가 실패해도 마지막 성공 prefix를 보존한다.
- [x] Mach-O read helper를 공용 bounds-checked reader로 추출한다.

범위: `src/collectors/dylib.rs`, `src/preprocessors/symbolicate.rs`.

### P1-42. symbolication을 UUID·architecture·image 단위로 수행한다

- [x] thin Mach-O와 FAT/FAT64에서 대상 architecture slice를 bounds-check해 선택한다.
- [x] dSYM bundle의 임의 첫 file을 선택하지 않고 process/image UUID와 architecture로 매칭한다.
- [x] 하나의 dSYM loader를 모든 frame에 적용하지 않고 frame의 image에 해당하는 loader/slide를 사용한다.
- [x] re-symbolication은 file/line/column location을 하나의 unit으로 교체해 stale field가 섞이지 않게 한다.
- [x] image별 parse/match/resolve 실패를 typed diagnostics로 남긴다.

범위: `src/preprocessors/symbolicate.rs`, `src/cli/symbolicate.rs`.

### P1-43. 수집한 SHM context를 report에서 잃지 않는다

- [x] `session_id`, `session_start_ns`, heartbeat 관련 값, `settings.extra`를 typed report field에 매핑한다.
- [x] 값의 producer/source와 optional semantics를 문서화한다.
- [x] serializer, loader, CLI, docs가 같은 schema를 사용한다.

범위: `src/shm/reader.rs`, `src/preprocessors/report_formatter.rs`, report schema.

### P1-44. environment 수집의 출처·이름·기본값을 정확히 한다

- [x] monitor의 현재 environment를 임의 target environment라고 표현하지 않고, spawn 시 child에 전달한 snapshot을 수집한다.
- [x] child-only 추가 값과 child가 runtime에 변경한 값은 snapshot에 반영되지 않는다는 한계를 기록한다.
- [x] Darwin kernel release를 `os_version`이라고 부르지 말고 `kernel_release`로 바꾸거나 macOS product version을 별도 수집한다.
- [x] `MemoryCollector`의 실제 데이터 의존성이 없는 `ThreadCollector` dependency를 제거한다.

범위: `src/collectors/environment.rs`, `src/collectors/memory.rs`.

### P1-45. stack과 screenshot capture에 전역 budget을 둔다

- [ ] thread 수, thread당 stack bytes, 전체 stack bytes, capture deadline 상한을 둔다.
- [ ] screenshot은 최근 N개, 전체 bytes, decode/copy deadline으로 제한한다.
- [ ] screenshot `tier`를 우선순위 선택에 사용한다.
- [ ] budget 초과 시 deterministic truncation과 diagnostics를 남긴다.

범위: `src/collectors/thread.rs`, `src/shm/reader.rs`, capture policy.

### P1-46. attachment 입력을 fd 기반 allowlist 정책으로 제한한다

- [ ] child-controlled label과 extension에 safe-character/component allowlist를 적용한다.
- [ ] source는 허용 root 아래 regular file인지 canonicalization과 open 후 `fstat`로 검증한다.
- [ ] `O_NOFOLLOW`를 사용하고 symlink, device, directory를 거부한다.
- [ ] metadata 확인 후 path로 다시 copy하는 TOCTOU를 제거하고 열린 fd에서 capped streaming copy한다.
- [ ] destination도 predictable prefix/symlink 조작에 안전하게 만든다.
- [ ] 향후 uploader가 생기기 전후의 privacy/consent 경계를 문서화한다.

범위: `src/collectors/attachment.rs`, artifact store.

### P1-47. privacy sanitizer를 모든 serialize 경로에 일관되게 적용한다

- [ ] trailing slash가 없는 exact HOME, HOME 하위 path, username-only 값을 path-component 단위로 마스킹한다.
- [ ] `$USER`가 없으면 `$HOME`의 마지막 component 또는 `getpwuid`를 fallback으로 사용하고, macOS path의 case-insensitive matching을 지원할지 정책과 테스트를 둔다.
- [ ] environment는 denylist보다 최소 allowlist를 기본으로 하고 URL userinfo, cookie, DSN, database URL, key material을 다룬다.
- [ ] breadcrumb file/message, annotations, attachment original path, thread name, hostname, raw dump에 같은 privacy redaction 정책을 적용한다.
- [ ] feedback은 Sanitizer 실행 뒤 JSON을 patch하므로 feedback 전용 정제 또는 final serialization 직전 sanitizer를 적용한다.
- [ ] hostname과 screenshot pixel은 기본 제외·opt-in·별도 redaction 정책 중 명시된 방식을 사용한다.
- [ ] screenshot 수집이 기본 활성이라면 feedback/consent UI에서 포함 여부를 분명히 알리고 선택 가능하게 한다.
- [ ] exact HOME, nested path, username-only value, missing USER를 테스트한다.

범위: `src/preprocessors/sanitizer.rs`, environment/screenshot collectors, report formatter.

### P1-48. feedback helper의 pipe와 timeout을 교착 없이 처리한다

- [ ] helper 실행 중 stdout/stderr를 concurrent하게 drain하거나 bounded file/buffer로 받는다.
- [ ] pipe capacity를 넘는 출력 때문에 helper 종료와 parent wait가 서로 기다리지 않게 한다.
- [ ] timeout 시 helper process를 정리하고 partial output과 timeout diagnostics를 보존한다.
- [ ] feedback UI는 critical capture/Mach reply 경로 밖에서만 실행한다.

범위: `src/postprocessors/feedback.rs`.

### P1-49. report-controlled 문자열을 terminal-safe하게 출력한다

- [ ] analyze, stack, symbolicate, log/notification 경로에서 ESC/OSC와 비출력 control character를 escape한다.
- [ ] JSON serialization 자체의 escaping과 terminal rendering을 구분한다.
- [ ] serde가 안전하게 escape한 JSON 값을 terminal 안전성만을 이유로 삭제하거나 의미 변경하지 않는다.
- [ ] annotation, thread name, attachment label, process name에 ANSI injection 회귀 테스트를 추가한다.

범위: `src/cli`, logging/notifier renderers.

### P1-50. dialog 실행 경로를 신뢰 경계로 취급한다

- [ ] `CRASH_MONITOR_DIALOG_BIN`은 존재 여부뿐 아니라 owner, regular-file 여부, 허용 경로, code signature를 검증한다.
- [ ] production에서 임의 environment override를 금지하거나 test-only feature로 제한한다.
- [ ] UI dialog helper는 monitor의 debugger entitlement를 공유하지 않고 최소 권한 파일로 별도 서명한다.

범위: `src/pipeline/mod.rs`, `Makefile`, entitlement files.

### P1-51. system notifier의 probe와 결과 검증을 고친다

- [ ] constructor의 동기 `osascript` probe를 제거하거나 lazy하게 실행한다.
- [ ] spawn 성공만으로 notification 성공으로 기록하지 않고 exit status와 stderr를 확인한다.
- [x] notifier 실패를 final diagnostics에 남긴다.

범위: `src/notifiers/system.rs`.

## P2 — 구조와 설정

### P2-01. crate와 module 경계를 실행 책임에 맞게 나눈다

- [ ] `main.rs`와 `lib.rs`가 같은 module tree를 각각 선언하지 않고 binary가 library API를 사용하게 한다.
- [ ] 최소한 `crash-report-core`, `crash-capture-macos`, `crash-monitor-cli`로 분리해 offline report tooling이 macOS compile error에 묶이지 않게 한다.
- [ ] producer SDK가 필요하면 별도 crate/package로 둔다.
- [ ] low-level FFI/SHM wildcard re-export를 제거하고 public API를 최소화한다.
- [ ] test-support API는 feature 또는 doc-hidden internal surface로 격리한다.

범위: `src/main.rs`, `src/lib.rs`, workspace manifests.

### P2-02. pipeline layering과 composition root를 분리한다

- [ ] report model이 formatter 구현을 import하고 formatter가 report model을 다시 import하는 cycle을 제거한다.
- [ ] report model, formatting, orchestration, default plugin assembly를 별도 module로 나눈다.
- [ ] platform-neutral trait/event loop에서 `mach_port_t`를 제거하고 opaque `TaskHandle`/capture context를 사용한다.
- [ ] `Pipeline` public mutable field를 private immutable state와 validated builder로 바꾼다.
- [ ] builder가 항상 dependency/order validation을 실행하게 한다.

범위: `src/pipeline`, `src/preprocessors/report_formatter.rs`, `src/event_loop.rs`.

### P2-03. `ReportContext`와 `ArtifactStore`를 공통 기반으로 둔다

- [ ] `ReportId`, output root, manifest, privacy policy, byte budget, deadlines를 immutable context에 묶는다.
- [ ] collector/postprocessor가 global path 계산을 다시 하지 않고 context/store만 사용하게 한다.
- [ ] artifact 등록, atomic commit, cleanup, retention을 store의 단일 contract로 제공한다.
- [ ] logical report transaction의 상태 전이를 타입과 문서로 정의한다.

범위: pipeline/report/artifact modules.

### P2-04. child와 capture resource를 supervisor state machine으로 관리한다

- [ ] PID, process group, task port, exception port, listener, SHM, suspend guard의 소유 상태를 한곳에서 관리한다.
- [ ] start, monitoring, capturing, finalizing, terminating, reaped 상태와 허용 전이를 정의한다.
- [ ] 각 상태에서 오류·signal·panic이 발생해도 정리 순서가 결정적이게 한다.

범위: `src/main.rs`, `src/event_loop.rs`, platform guards.

### P2-05. config는 한 번 load·validate한 immutable 값만 사용한다

- [ ] main과 pipeline factory의 이중 `load_config`를 제거한다.
- [ ] missing file과 malformed/read-error를 구분하고 오류를 조용히 default로 바꾸지 않는다.
- [ ] unknown field를 warning 또는 deny하고 모든 numeric range를 validate한다.
- [ ] `max_events=0`, retention 0, ANR threshold/interval, timeout 등 sentinel과 범위를 문서화한다.
- [ ] JSON을 primary source로 하고 ANR/timeout environment override는 명시적인 test/ops override로 제한한다.
- [ ] `check-config` command를 제공하고 `config::is_enabled`는 실제로 사용하거나 제거한다.

범위: `src/config.rs`, `src/main.rs`, pipeline factory.

### P2-06. plugin identity와 dependency 모델을 typed하게 만든다

- [ ] 문자열 이름 대신 stable typed `PluginId`를 사용한다.
- [ ] dependency graph를 topological order로 계산하고 cycle을 진단한다.
- [ ] hard data dependency와 단순 실행 순서를 별개 타입/field로 표현한다.
- [ ] 모든 category에 같은 uniqueness, dependency, skip contract를 적용한다.

범위: pipeline traits/types/builder.

### P2-07. app-specific SHM field를 generic extension schema로 옮긴다

- [ ] TOOL/WORLD/UNDO/MESH, palette/history/world-bound와 voxel category를 core schema에서 제거하거나 versioned extension으로 격리한다.
- [ ] generic annotations/settings/category extension contract를 정의한다.
- [ ] context collector와 dialog/help의 이전 host-project 예시를 generic wording으로 바꾼다.

범위: `schema/crash_shm.h`, formatter/collectors/docs.

### P2-08. 실제 producer SDK와 올바른 SSOT 문서를 제공한다

- [ ] heartbeat, breadcrumb, context, attachment, screenshot publication API를 C/C++/Rust 중 지원 대상 언어에 제공한다.
- [ ] 존재하지 않는 `sut_crash_reporter.h` 참조를 실제 `schema/crash_shm.h`와 binding flow로 교체한다.
- [ ] release/acquire, ready handshake, version negotiation, size/alignment contract를 SDK와 함께 테스트한다.

범위: schema, `src/shm/mod.rs`, producer packages, integration docs.

### P2-09. structured tracing과 typed error를 도입한다

- [ ] monitor 내부 `eprintln!`을 level/target이 있는 structured tracing으로 교체한다.
- [ ] user-facing CLI stderr와 monitor operational log를 분리한다.
- [ ] platform/plugin/path/SHM/artifact의 `Result<_, String>`을 영역별 typed error로 바꾼다.
- [ ] config의 `Option`+silent default 경로는 `Result<ValidatedConfig, ConfigError>`로 바꿔 오류를 새로 보존한다.
- [ ] 모든 log/error에 가능한 경우 `ReportId`, PID, stage, plugin ID를 포함한다.
- [ ] 낡은 `#[allow(dead_code)]`, Phase 주석, 존재하지 않는 설계 문서 링크를 제거한다.

범위: repository-wide, 특히 platform/pipeline/main.

### P2-10. generated binding과 workspace 설정의 drift를 막는다

- [ ] generated binding을 check-in하고 CI에서 schema drift를 검사하거나 bindgen/libclang toolchain을 명확히 pin한다.
- [ ] 약 50MB screenshot struct에 bindgen이 `Copy`/`Debug`를 파생하지 않게 `no_copy`/`no_debug`를 설정한다.
- [ ] `SutCrumbState` 등 다른 수백 KiB generated struct도 크기 기준으로 `no_copy`/`no_debug` 적용 여부를 검토한다.
- [ ] `workspace.package`, shared dependencies, shared lints를 member crate가 상속하게 한다.
- [ ] 사용하지 않는 `uuid`는 제거하거나 실제 `ReportId` 구현에 사용한다.

범위: `build.rs`, `Cargo.toml`, member manifests.

### P2-11. dialog UI와 mock의 CLI contract를 통일한다

- [ ] wrapping label로 만든 입력란을 표준 editable `NSTextField` 또는 `NSTextView`로 바꾼다.
- [ ] real/mock helper가 `--mock-input`, `--dry-run`, skip exit semantics와 출력 schema를 공유한다.
- [ ] dialog title에 이전 제품명을 하드코딩하지 않고 process name 또는 설정값을 사용한다.

범위: `crates/crash_dialog_macos`, `crates/crash_dialog_mock`.

## P2 — CLI 품질과 유지보수

### P2-12. analyze 출력에서 JSON 값과 민감 문자열을 정확히 다룬다

- [ ] string이 아닌 annotation을 빈 문자열로 만들지 않고 JSON 표현으로 출력한다.
- [ ] context/report field의 누락과 type mismatch를 명확히 표시한다.
- [ ] output 함수가 `Write` sink를 받아 실제 formatting을 테스트할 수 있게 한다.
- [ ] test fixture를 현재 annotations/report shape로 갱신한다.

범위: `src/cli/analyze.rs`, analyze tests.

### P2-13. stack CLI의 경계·출력·메모리 사용을 고친다

- [ ] thread가 0개일 때 `0..0` range가 아니라 전용 no-threads 메시지를 낸다.
- [ ] header에 declared size가 아니라 실제 decoded byte 길이를 표시하고 mismatch를 경고한다.
- [ ] 최대 입력의 전체 hexdump 문자열을 메모리에 만들지 않고 locked writer에 line 단위로 출력한다.
- [ ] import는 파일 상단, test module은 하단에 두어 module layout을 정리한다.
- [ ] `src/pipeline/safety.rs` 중간의 `#[cfg(test)] mod tests` 선언도 파일 끝으로 이동한다.

범위: `src/cli/stack.rs`.

### P2-14. symbolicate CLI의 선택과 성공 피드백을 명확히 한다

- [ ] DWARF directory의 임의 첫 file을 선택하지 않고 bundle/process/image와 매칭하며 ambiguity는 오류로 낸다.
- [ ] hidden/irrelevant file을 건너뛴다.
- [ ] `--output` 성공 시 destination path와 resolved frame 수를 출력한다.
- [ ] help에 기본 output이 JSON이 아니라 human-readable summary임을 명확히 적는다.

범위: `src/cli/symbolicate.rs`, `src/main.rs` CLI definitions.

### P2-15. main CLI help와 exit-code contract를 정리한다

- [ ] subcommand 없는 수동 usage 문자열을 제거하고 clap-generated help를 사용한다.
- [ ] usage 오류는 관례적인 별도 exit code를 사용한다.
- [ ] monitor internal failure, child failure, detected crash, normal completion의 exit code를 문서화한다.
- [ ] 성공·실패 메시지가 실제 build target과 command 이름을 가리키게 한다.

범위: `src/main.rs`, README/help tests.

## P2 — 테스트, CI, build, packaging

이 절의 실제 OS signal/ANR E2E, instrumented monitor coverage, test isolation 항목은 분류상 품질 작업이지만, 대응하는 P0/P1 항목을 완료하기 위한 release gate로 간주한다.

### P2-16. macOS CI와 privileged E2E gate를 만든다

- [ ] macOS ARM64 fast job에서 format, strict Clippy, unit, integration, schema drift를 실행한다.
- [ ] entitlement/signing이 준비된 required E2E job을 별도로 둔다.
- [ ] privileged prerequisite가 없는 일반 CI에서는 E2E를 명시적으로 ignored로 표시한다.
- [ ] release branch에서는 required E2E 미실행을 성공으로 간주하지 않는다.

범위: CI workflow, Makefile/Cargo commands.

### P2-17. E2E prerequisite와 helper 준비를 결정적으로 만든다

- [ ] fixture, monitor binary, entitlement, signing identity, mock dialog가 없을 때 단순 return으로 pass 처리하지 않는다.
- [ ] `E2E_REQUIRED=1`이면 prerequisite 부재를 실패시키고, 아니면 명시적인 skip 이유를 출력한다.
- [ ] mock dialog가 없을 때 real UI를 띄워 최대 수분 block하지 않게 한다.
- [ ] feedback integration test가 helper binary의 우연한 사전 build에 의존하지 않게 setup에서 build/locate한다.
- [ ] 안내 메시지의 존재하지 않는 make target 이름을 실제 target으로 교체한다.

범위: `tests/e2e/e2e_tests.rs`, `tests/integration/cli_feedback_test.rs`, Makefile.

### P2-18. 실제 OS signal 경로를 E2E로 검증한다

- [ ] 실제 SIGUSR1 snapshot을 통해 signal handler, self-pipe, event loop, report 생성까지 검증한다.
- [ ] child SIGKILL을 통해 waitpid와 `PossibleOom` 분류를 검증한다.
- [ ] SIGSEGV, SIGABRT, clean exit, nonzero exit, 다른 fatal signal을 각각 검증한다.
- [ ] report type, termination metadata, unique ID, 최종 artifact 위치까지 단언한다.

범위: `tests/e2e/e2e_tests.rs`, crash fixture.

### P2-19. ANR 테스트를 deadline polling과 명시적 cleanup으로 바꾼다

- [ ] 고정 3초 sleep 대신 report/manifest 완성을 bounded deadline polling으로 기다린다.
- [ ] monitor와 child PID/process group을 test teardown에서 직접 정리하고 잔존 process/SHM이 없는지 확인한다.
- [ ] graceful SIGTERM cleanup 구현 전에는 monitor에 SIGTERM만 보내면 child도 정리된다고 가정하지 않는다.
- [ ] fixture 자체 deadline을 두어 실패한 테스트도 무기한 orphan을 남기지 않게 한다.

범위: `tests/e2e/e2e_tests.rs`, fixture lifecycle.

### P2-20. event-loop ANR wiring을 결정론적으로 통합 테스트한다

- [ ] injectable clock/짧은 ANR config/SHM fixture로 elapsed→heartbeat read→ANR event 경로를 검증한다.
- [ ] monitor가 소비한 시간을 제외하는 accounting을 단언한다.
- [ ] `ChildGone`/unknown status와 clean exit 0을 구분하는 경로도 포함한다.

범위: `tests/integration/event_loop_test.rs`, watchdog tests.

### P2-21. timeout 테스트를 실제 차단 동작으로 교체한다

- [ ] 단순 flag/alarm 호출 여부 테스트를 실제 blocking syscall, CPU loop, retrying I/O worker 테스트로 바꾼다.
- [ ] kill 가능한 subprocess harness로 test suite 자체가 hang하지 않게 한다.
- [ ] timeout, cancellation, worker crash, 정상 완료 결과를 구분한다.

범위: pipeline safety unit/integration tests.

### P2-22. test command가 workspace의 실제 target을 모두 실행하게 한다

- [ ] 수동 test target 목록 대신 `cargo test --workspace --all-targets`에 준하는 future-proof command를 사용한다.
- [ ] 누락된 analyze, stack, symbolicate, feedback/zip CLI integration target을 포함한다.
- [ ] 실제 binary process를 실행해 clap parsing, stdout, stderr, exit status를 검증한다.
- [ ] exit code만이 아니라 선택된 report/thread/field와 output marker를 단언한다.

범위: `Makefile`, CLI integration tests.

### P2-23. E2E coverage가 실제 monitor binary를 계측하게 한다

- [ ] instrumented monitor binary를 build·sign하고 그 경로를 E2E에 주입한다.
- [ ] 별도 release binary를 spawn해 coverage가 test harness에만 쌓이는 현상을 제거한다.
- [ ] `main.rs`, FFI, path handling을 blanket exclude하지 않고 실행 가능한 coverage 또는 별도 gate를 둔다.
- [ ] coverage target의 한계를 문서화하고 측정되지 않는 코드를 숫자에 포함한 것처럼 표시하지 않는다.

범위: `Makefile`, E2E binary resolution.

### P2-24. collector와 parser의 경계 테스트를 보강한다

- [ ] attachment, breadcrumb, context, screenshot collector의 success, partial failure, bounds를 테스트한다.
- [ ] Mach-O symbol parser에 thin/FAT, malformed command, truncated string/table, invalid range fixture를 추가한다.
- [ ] `find_symbol`의 exact match, empty symbol array, 1MiB 거리 초과, underscore stripping 경계를 테스트한다.
- [ ] same-second snapshot test는 `>=1`이 아니라 정확히 2개의 고유 report와 분리된 artifact를 단언한다.
- [ ] screenshot tier/limit, attachment name/size/symlink, empty fingerprint를 포함한다.

범위: unit/integration collector and parser tests.

### P2-25. fuzz, property, fault-injection suite를 추가한다

- [ ] malformed Mach message, Mach-O, SHM header/ring을 fuzz한다.
- [ ] plugin toggle/dependency graph와 schema offsets를 property test한다.
- [ ] path traversal, symlink swap, disk full, permission denied, cross-device move, ZIP bomb/failure를 fault-inject한다.
- [ ] capture/finalize 각 단계에서 process kill 후 recovery를 검증한다.

범위: parser crates, pipeline/artifact store, dedicated fuzz targets.

### P2-26. test의 process-global state와 고정 경로를 제거한다

- [ ] environment filter를 pure function으로 만들고 synthetic environment를 주입한다.
- [ ] process-global `set_var`에 의존하는 병렬 테스트를 제거한다.
- [ ] 고정 temp path와 공유 `target/test-crash-data` 대신 `tempfile`/unique directory를 사용한다.
- [ ] SHM test name은 실제 PID+random nonce/counter로 global POSIX namespace 충돌을 막는다.
- [ ] raw cleanup, symbolicate, session tests도 각각 격리된 tempdir를 사용한다.

범위: tests repository-wide.

### P2-27. SHM producer와 layout fixture를 production contract에 묶는다

- [ ] E2E C producer가 `fstat`으로 mapping size와 schema version을 확인한 뒤 offset에 쓴다.
- [ ] integration test의 수동 숫자 offset을 `offset_of!`/generated constants로 교체한다.
- [ ] 수동 테스트가 유일하게 검증하던 `SutBreadcrumb` field와 `SutCrumbRing.buf/write_idx/count` numeric offsets는 production compile-time assertion으로 옮긴다.
- [ ] breadcrumb capacity wrap order, timestamp 0 torn entry, corrupted/huge write index를 테스트한다.

범위: `tests/e2e/fixtures/crash_app.c`, SHM integration/unit tests.

### P2-28. config와 pipeline factory 테스트를 실제 loader 기준으로 만든다

- [ ] `load_config_from_path` happy path를 직접 호출하고 모든 주요 field를 단언한다.
- [ ] malformed, unknown field, range violation, missing file을 각각 테스트한다.
- [ ] default pipeline test가 공유 data directory의 외부 config 상태에 의존하지 않고 config를 직접 주입받게 한다.

범위: config and pipeline unit tests.

### P2-29. postprocessor 테스트가 실제 부수효과를 검증하게 한다

- [ ] SessionRecorder가 JSONL 내용, lock 생성/삭제, rotation behavior를 tempdir에서 단언한다.
- [ ] RawCleanup이 파일 삭제와 in-memory path 갱신을 모두 단언한다.
- [ ] PNG/ZIP/retention test가 atomicity, manifest, logical-report grouping, symlink 거부를 검증한다.

범위: postprocessor unit/integration tests.

### P2-30. lint gate를 workspace 전체에 적용한다

- [ ] Clippy를 `--workspace --all-targets --all-features`와 warning deny 정책으로 실행한다.
- [ ] workspace 공통 lint에서 `unsafe_op_in_unsafe_fn`을 실제 `deny`로 올리고 member가 상속하는지 검증한다.
- [ ] global dead-code allow를 제거하고 필요한 예외만 좁게 둔다.
- [ ] format은 workspace member까지 검사한다는 실제 동작을 반영해 명령을 명확히 유지한다.
- [ ] local Make target과 CI gate가 같은 범위를 검사하게 한다.

범위: `Makefile`, CI, workspace lints.

### P2-31. compile, sign, package, E2E target을 분리한다

- [ ] signing identity가 없어도 `build-unsigned`는 성공하게 한다.
- [ ] `sign`, `package`, `e2e`를 별도 target으로 분리하고 필요한 identity를 실행 전에 검사한다.
- [ ] ad-hoc signing을 허용할 범위와 privileged E2E에 필요한 정식 signing을 구분한다.
- [ ] 긴 compile 뒤에야 signing 오류가 나는 흐름을 제거한다.

범위: `Makefile`, release scripts.

### P2-32. test-only dialog mock을 production artifact에서 분리한다

- [ ] production workspace/default build/package에 `crash_dialog_mock`이 포함되지 않게 한다.
- [ ] test에서는 명시적인 dev dependency/fixture로 build한다.
- [ ] production package manifest에 허용 binary 목록을 둔다.

범위: workspace/package manifests.

### P2-33. toolchain과 coverage tool discovery를 이식 가능하게 만든다

- [ ] `/opt/homebrew`에 고정된 LLVM 경로 대신 rustup `llvm-tools` 또는 `brew --prefix llvm`/명시 override를 사용한다.
- [ ] `rust-version`과 `rust-toolchain.toml`로 지원 toolchain을 고정한다.
- [ ] libclang/bindgen 요구사항을 developer setup과 CI에 명시한다.

범위: `Makefile`, Cargo/toolchain files, contributor docs.

### P2-34. package metadata와 프로젝트 운영 문서를 완성한다

- [ ] license, repository, homepage, documentation, include/exclude를 의도에 맞게 채운다.
- [ ] 비공개 package면 `publish=false`, 공개 package면 실제 publish contract를 명시한다.
- [ ] `SECURITY.md`, `CONTRIBUTING.md`, release checklist를 추가한다.
- [ ] member crate가 workspace metadata를 일관되게 상속하는지 검사한다.

범위: Cargo manifests, repository root docs.

### P2-35. 배포 artifact contract를 정의한다

- [ ] monitor와 dialog의 설치 위치, 상대 탐색 규칙, signature/entitlement를 정의한다.
- [ ] update 방식, checksum, dSYM 보존·매칭, 최소 macOS/architecture를 명시한다.
- [ ] package 내용과 서명을 자동 검증하는 release test를 추가한다.

범위: packaging scripts and release docs.

### P2-36. dependency 갱신은 근거 기반 유지보수로 수행한다

- [ ] advisory, changelog, MSRV, API break, migration cost를 검토하는 정기 dependency audit를 둔다.
- [ ] 단순히 최신 major가 아니라는 이유만으로 confirmed defect로 취급하거나 일괄 업그레이드하지 않는다.

범위: Cargo dependencies, dependency-review process.

### P2-37. disk-space 계산을 64-bit와 overflow에 안전하게 만든다

- [ ] macOS에서 64-bit `statfs` 계열을 사용하거나 block count 곱셈의 overflow/saturation을 처리한다.
- [ ] 매우 큰 volume과 API 오류를 테스트한다.

범위: `src/filters/disk_space.rs`. 이 항목은 일반 환경의 즉시 장애라기보다 portability hardening이다.

## P2 — 문서와 제품 분리 정리

### P2-38. 이전 host project의 이름·경로·개념을 제거한다

- [ ] `.cargo/config.toml`, `src/main.rs`, dialog title/help, tests/docs의 Model Block Builder, voxel, F8, 이전 tool path를 Crash Monitor의 중립 명칭으로 바꾼다.
- [ ] 존재하지 않는 make target과 `docs/plans/crash_reporter.md` 참조를 실제 command/document로 교체한다.
- [ ] 의미 없는 Phase 주석과 이미 완료된 roadmap 표현을 현재 구조 설명으로 바꾼다.
- [ ] schema의 app-specific field 정리와 문서 용어 정리를 함께 수행한다.

범위: repository-wide comments, config, UI strings, docs.

### P2-39. 실제 startup과 artifact lifecycle을 문서화한다

- [ ] “fork+exec” 설명을 실제 `posix_spawn`과 exception-port 설정 흐름으로 고친다.
- [ ] 기본 pipeline의 최종 산출물이 `pending` JSON이 아니라 `sent`의 ZIP이라는 사실과 중간 상태를 설명한다.
- [ ] temp→pending→archive→sent→retention 및 recovery 상태를 diagram/문장으로 동기화한다.
- [ ] snapshot, crash, ANR, exit failure별 artifact 차이를 설명한다.

범위: `README.md`, `docs/architecture.md`, report/pipeline docs.

### P2-40. config reference를 실행 가능한 수준으로 완성한다

- [ ] 실제 JSON example과 모든 default를 제공한다.
- [ ] value range, 0/sentinel 의미, plugin dependency, trigger별 enabled semantics를 적는다.
- [ ] global disable과 emergency evidence 정책을 명확히 구분한다.
- [ ] OOM/ANR opt-in, environment override 우선순위, invalid config 처리 방식을 설명한다.

범위: README/config documentation.

### P2-41. pipeline과 timeout 문서를 구현과 일치시킨다

- [ ] duplicate detection을 Filter가 아니라 PreProcessor로 분류하고 short-circuit 시점을 설명한다.
- [ ] Stage 1, collector, preprocessor, finalizer, notifier 순서와 동기/비동기 경계를 정확히 적는다.
- [ ] timeout 문서에는 subprocess/deadline 구현의 실제 보장만 기술한다.
- [ ] SHM 문서의 acquire/volatile 자기모순을 제거하고 실제 atomic contract와 일치시킨다.

범위: `docs/pipeline.md`, `docs/shared-memory.md`.

### P2-42. versioned report schema 문서를 serializer와 동기화한다

- [ ] `build`, `settings_snapshot`, `environment`, `user_feedback`, diagnostics 등 실제 top-level field를 모두 문서화한다.
- [ ] field type, optionality, privacy classification, producer, version introduction을 명시한다.
- [ ] postprocessor/notifier 결과가 어디에 최종 기록되는지 설명한다.
- [ ] example fixture 또는 schema test로 문서와 serializer drift를 감지한다.

범위: `docs/reports.md`, report fixtures/schema.

### P2-43. data directory 보안 정책을 코드와 문서에 맞춘다

- [ ] 이미 문서화된 absolute path 요구를 유지하고 owner, permission, symlink, ACL, existing-directory 처리 정책을 보완한다.
- [ ] directory/file mode, atomic write, cleanup, multi-instance locking을 설명한다.
- [ ] 사용자가 잘못된 path를 지정했을 때의 오류와 복구 방법을 제공한다.

범위: `docs/integration.md`, path/config code.

### P2-44. privacy·retention·공유 지침을 제공한다

- [ ] environment, stack/memory, screenshot, breadcrumb, attachment path/content, hostname의 민감성을 분류한다.
- [ ] 기본 수집, opt-in, consent, redaction, retention, encryption 정책을 적는다.
- [ ] report를 support/vendor와 공유하기 전 검토·정제하는 절차를 제공한다.
- [ ] raw와 final archive의 차이 및 uploader가 추가될 때 필요한 별도 동의를 설명한다.

범위: privacy/security documentation.

### P2-45. 운영 troubleshooting runbook을 만든다

- [ ] entitlement와 `task_for_pid` 실패 진단 절차를 제공한다.
- [ ] signing identity, helper signature, architecture mismatch를 확인하는 절차를 제공한다.
- [ ] orphan child/process group, stale SHM, incomplete `pending`, failed archive를 확인·복구하는 절차를 제공한다.
- [ ] 안전하게 수집을 끄고 기존 민감 report를 정리하는 절차를 제공한다.

범위: operations/troubleshooting documentation.

### P2-46. capture helper의 `waitpid` 소유권과 `ECHILD` 정책을 통일한다

- [ ] 정상 완료 poll, timeout 후 kill/reap, capability handoff 실패 정리에서 공통 typed reap 결과와 하나의 `ECHILD` 정책을 사용한다.
- [ ] capture helper를 reap하는 주체가 정확히 하나라는 invariant를 명시하고, 별도 global/late reaper가 같은 PID를 소비하지 못하게 한다.
- [ ] strict capture 경계에서 `ECHILD`를 단순 성공이나 `TimedOut` 증거로 사용하지 않고 wait 소유권 상실로 진단해 `CleanupUnproven` containment를 적용한다.
- [ ] 정상 완료의 exit status 확인과 helper/Mach-right cleanup 증명을 구분해, status를 얻지 못한 결과가 성공으로 decode되지 않게 한다.
- [ ] 세 경로에 `ECHILD`를 fault-inject해 target이 resume되지 않고 최소 evidence와 supervisor diagnostics가 보존되는지 테스트한다.

범위: `src/pipeline/capture_isolation.rs`, `src/platform/macos/ffi/capture_spawn.rs`, capture worker tests.

### P2-47. capture helper에 상속되는 file descriptor를 allowlist로 제한한다

- [ ] Darwin `POSIX_SPAWN_CLOEXEC_DEFAULT`를 capture-helper spawn attribute에 적용해 file action으로 명시하지 않은 descriptor를 모두 닫는다.
- [ ] result channel FD 3만 필수 상속하고, stderr가 필요하면 `addinherit_np`로 의도적인 예외를 선언하거나 helper 오류를 bounded result envelope로 전달한다.
- [ ] result source FD가 3인 경우의 duplicate/close 순서와 `FD_CLOEXEC` 처리를 유지해 exec 뒤 FD 3이 정확히 하나의 owner를 갖게 한다.
- [ ] 의도적으로 `CLOEXEC`가 아닌 sentinel pipe/file/lock FD를 연 상태에서 helper를 spawn해 helper에서는 `EBADF`이고 parent 수명과 EOF/lock을 연장하지 않는지 검증한다.
- [ ] 불필요한 FD가 닫힌 상태에서도 capability handoff, bounded result write, timeout kill/reap이 모두 동작하는 실제 exec 통합 테스트를 추가한다.

범위: `src/platform/macos/ffi/capture_spawn.rs`, `tests/integration/capture_isolation_test.rs`.

## 전체 완료 조건

- [ ] `exit(1)`, uncaught signal, Mach exception, possible OOM, ANR, snapshot이 각각 고유 `ReportId`와 올바른 termination metadata로 보고된다.
- [ ] Mach exception 수신 뒤 bounded capture, resume, reply가 설정 deadline 안에 끝난다.
- [ ] feedback, ZIP, notifier hang이 child resume 또는 Mach reply를 지연하지 않는다.
- [ ] suspend 성공 경로마다 resume가 정확히 한 번 실행되고 실패가 관측 가능하다.
- [ ] 동일 초 다중 event와 stage 간 시간 경계에서도 artifact가 섞이거나 overwrite되지 않는다.
- [ ] global disable 상태에서는 합의된 명시 정책 외의 raw/JSON/artifact가 생성되지 않는다.
- [ ] SHM producer 미연동 child와 monitor 자체 처리시간이 ANR로 오진되지 않는다.
- [ ] 모든 report directory/file이 각각 0700/0600이고 symlink/owner 검증을 통과한다.
- [ ] strict workspace Clippy, unit, integration, required E2E, schema compatibility, recovery fault test가 CI에서 통과한다.
- [ ] capture/finalize 각 단계에서 monitor를 강제 종료한 뒤 재시작해도 orphan child, stale SHM, 노출된 partial report가 남지 않는다.

## 구현 시 전제로 삼지 말아야 할 주장

아래 항목은 후속 검증에서 무효 또는 과장으로 확인됐다. 관련 영역을 수정할 때 잘못된 원인이나 영향 범위를 다시 도입하지 않는다.

| 잘못된 전제 | 구현에 사용할 정확한 조건 |
|---|---|
| SIGALRM/SIGUSR1이 현재 option의 `mach_msg` receive를 곧바로 `MACH_RCV_INTERRUPTED`로 끝내 listener를 죽인다. | 현재 receive/send option에는 interrupt flag가 없어 wrapper가 재시작한다. 실제 receive failure supervision과 process-global timeout의 불신뢰성은 별개 문제다. |
| monitor 기동 전에 만든 같은 이름의 SHM object가 그대로 재사용된다. | 기존 이름은 open 전에 unlink된다. 남는 문제는 predictable PID name, unlink/open TOCTOU, `O_EXCL` 부재와 실패 경로 정리다. |
| workspace root의 현재 `cargo fmt -- --check`가 member crate를 검사하지 않는다. | formatting은 member도 검사한다. 실제 lint 공백은 Clippy의 workspace/all-target/all-feature 범위와 broad allow다. |
| ANR test가 monitor에 SIGTERM을 보내면 현재 child도 정리된다. | 현재 graceful SIGTERM cleanup이 먼저 구현돼야 한다. 그 전에는 test가 child/process group을 직접 정리해야 한다. |
| monitor environment는 spawn된 target environment와 전혀 무관하다. | spawn 시점에는 대부분 같지만 child-only 추가 값과 runtime 변경은 다를 수 있다. 전달한 child snapshot을 source로 쓰는 것이 정확하다. |
| 표준 `std::env::set_var`와 `std::env::vars`끼리 자체 data race가 발생한다. | 표준 API끼리는 내부 동기화를 사용한다. 유효한 test 위험은 chrono/CoreFoundation 같은 non-std `getenv` 사용자와 process-global mutation이 병렬 실행되는 경우다. synthetic environment 주입으로 제거한다. |
| in-memory duplicate map이 재시작하는 fatal crash loop를 영구 억제한다. | fatal crash 뒤 monitor가 종료되어 map도 사라진다. sliding suppression 문제는 같은 monitor lifetime의 반복 snapshot/ANR에 해당한다. 재시작 crash-loop에는 오히려 persistent state 부재가 문제다. |
| ANR test가 write 중인 partial final ZIP을 읽어 parse panic한다. | ZIP은 `.tmp` 뒤 rename되므로 final search가 partial temp를 읽지 않는다. 실제 문제는 고정 sleep으로 report가 아직 없을 수 있다는 flakiness다. |
| 설정 parse 실패 시 모든 plugin이 default enabled가 된다. | 대부분의 collector/postprocessor가 default로 돌아가지만 system notification 등 예외가 있다. 핵심은 malformed config가 조용히 fallback된다는 점이다. |
| 모든 artifact가 항상 0755/0644 또는 world-readable이다. | 실제 mode는 umask와 상위 ACL에 달려 있다. private 0700/0600을 코드가 강제하지 않는 것이 결함이다. |
| screenshot layout 검증이 전혀 없다. | bindgen layout assertion은 존재한다. handwritten section-size 공식과 generated struct 사이 직접 대조 및 slot/padding drift 검증이 부족하다. |
| 현재 유일한 `get_task_info_bytes` caller에서 silent truncation이 이미 발생한다. | 현재 caller size는 맞는다. alignment와 returned-count를 숨기는 API contract가 future safety 문제다. |
| exec child가 monitor의 모든 custom signal disposition을 그대로 상속한다. | custom handler는 exec에서 default가 되지만 blocked mask와 `SIG_IGN`은 정책적으로 초기화해야 한다. |
| empty fingerprint가 서로 다른 fatal crash를 계속 억제한다. | 동일 monitor의 fatal crash는 보통 한 번이다. 실제 유실 가능성은 앞선 snapshot/ANR과 뒤의 fatal crash가 같은 empty fingerprint/window를 공유할 때다. |
| attachment label에 단순 `../`만 넣으면 항상 즉시 output root 밖으로 탈출한다. | prefixed path 때문에 항상 성공하지는 않는다. predictable directory/symlink 조건에서 위험이 커지므로 component validation과 no-follow가 필요하다. |
| 현재 repository만으로 attachment가 외부 전송된다. | uploader는 없다. 임의 file 수집/confused-deputy 위험은 유효하고 외부 유출은 downstream upload가 추가될 때 조건부다. |
| exact HOME 노출은 monitor integrity를 깨는 Critical이다. | 실제 privacy bug지만 일반적으로 P1 privacy 결함으로 처리한다. |
| ZIP loop에서 모든 attachment buffer가 누적되어 수백 MB가 된다. | per-file buffer는 반복마다 해제된다. peak는 보통 가장 큰 단일 file이며 그래도 streaming으로 고쳐야 한다. |
| stale `raw_path`가 현재 public caller에 노출된다. | `ReportResult`는 현재 내부 local이다. path를 `None`으로 갱신하는 것은 유효한 내부 contract 정리다. |
| 32-bit filesystem block count가 일반 환경에서 즉시 운영 장애를 낸다. | 매우 큰 volume과 OS saturation semantics에 좌우되는 portability hardening이다. |
| `mach_vm_region.object_name`이 현대 macOS에서 실질 port right를 계속 누수한다. | 현대 XNU는 이 out parameter에 null을 반환한다. exception task/thread right와 receive-right cleanup은 별도로 검증·관리한다. |
| 최신 major가 아닌 dependency 자체가 correctness/security defect다. | advisory, changelog, MSRV와 실제 수정 내용을 확인한 뒤 근거 기반으로 갱신한다. |
