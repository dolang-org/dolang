# Building CLI Tools

## Example

[`example/download.dol`](https://github.com/bkoropoff/dolang/blob/master/example/download.dol)
downloads files concurrently with progress reporting:

```
#!/usr/bin/env -S dolang --strict
import args progress http fs url strand

def download client url
  let name = url.name
  if !name
    throw "no identifiable file name: $url"
  client.get $url do |resp|
    let len = resp.headers.get content-length
    let success = false
    try
      fs.open $name w do |file| progress.show
        message: $name
        total: (len && int len)
        units: :BYTES:
        do |w| for chunk = resp.chunks()
          file.write $chunk
          w.delta $chunk.len
      success = true
    finally
      if !success
        fs.remove $name ignore: true

args.with
  help: Download files over HTTP(S)
  - opt: limit
    short: l
    type: $int
    help: Maximum concurrent downloads
    default: 4
  - arg: urls
    collect: true
    help: URLs to download
    type: $url.Url
  do |args| progress.with do
    let client = http.Client()
    strand.pool $args.limit $args.urls do |url|
      download $client $url
```

### Argument Parsing

[`args.with`](../api/args.md) describes the command line and invokes its block
with a record of converted values. `type:` specifies how to coerce the argument
from a string, e.g. `type: $url.Url` ensures that `download` receives URL
objects. `collect: true` gathers the remaining positional arguments into an
array.

### Progress Indicators

[`progress.with`](../api/progress/index.md#with-func) activates progress
rendering for the passed block scope.
[`progress.show`](../api/progress/index.md#show-func) creates a child
indicator.

### Structured Concurrency

[`strand.pool`](../api/strand/index.md#pool-count-input-func) feeds URLs
lazily to a worker pool of `args.limit` strands. The call is scoped, waiting for
all work to finish (or an uncaught error to propagate, in which case remaining
workers are canceled) and cleaning up the pool on exit.
