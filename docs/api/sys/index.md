# sys

The `sys` module exposes facts about the current target system and system error
types.

## Types

| Type                                                    | Description                                   |
| ------------------------------------------------------- | --------------------------------------------- |
| [`OsInfo`](./osinfo.md)                                 | Operating system target information           |
| [`CpuInfo`](./cpuinfo.md)                               | CPU target information                        |
| [`ErrorCode`](./error-code.md)                          | Native system error code                      |
| [`Error`](./error.md)                                   | Error raised for system and I/O failures      |
| [`NotFoundError`](./not-found-error.md)                 | Subtype for missing files, paths, or programs |
| [`PermissionDeniedError`](./permission-denied-error.md) | Subtype for permission failures               |
| [`AlreadyExistsError`](./already-exists-error.md)       | Subtype for existing-path conflicts           |
| [`TimedOutError`](./timed-out-error.md)                 | Subtype for timed-out system operations       |
| [`UnsupportedError`](./unsupported-error.md)            | Subtype for unsupported system operations     |

## Functions

### `os_info()`

Returns target operating system information.

```
if (sys.os_info().family == :WINDOWS:)
  echo "running on Windows"
```

### `cpu_info()`

Returns target CPU information.

```
let info = sys.cpu_info()
echo "running on $info.arch with $info.logical_count logical CPUs"
```
