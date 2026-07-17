# Examples

## Running External Programs

External programs can be run or imported via [`proc.run`](./api/proc-run.md).

```
# Run a program directly
run gcc -o main main.c -Wall -Werror

# Import specific programs as functions
import proc.run:
  - uname
  - git

# Capture output with `sub`
let kernel = sub do uname -r
echo "Running kernel: $kernel"
let branch = sub do git rev-parse --abbrev-ref HEAD
echo "On branch: $branch"
```

## Building Container Images

`example/cow.dol` searches OpenSUSE packages by name, then builds a Docker image
with the results installed:

```
#!/usr/bin/env -S dolang --strict
import xml progress docker
import proc.run:
  - zypper

def search name
  let doc = xml.from_str $ sub do zypper --xmlout search $name
  return
    for n = doc.traverse()
      if (type n xml.Node && n.tag == "solvable")
        $n["name"]

progress.with do docker.build
  from: opensuse/leap:15.6
  mounts:
    - type: cache
      target: /var/cache/zypp
  run: do progress.show message: "updating repos" do |_|
    zypper -n modifyrepo --all --keep-packages
    zypper -n refresh
  commit: After refresh
  run: do progress.show message: searching icon: 📦 do |w|
    let pkgs = search cow*
    w.total = pkgs.len
    for pkg = pkgs
      w.message = "install $pkg"
      zypper -n install $pkg
      w.delta()
    run cowsay MOO
  tag: opensuse-with-cows
```

Things to notice:

- `zypper` is imported as a callable function from `proc.run`
- `search` returns an array with vertical `for` layout
- `docker.build` intermixes declarative syntax and `do` blocks

## Concurrent File Downloader

`example/download.dol` downloads files over HTTP with progress bars and
configurable concurrency:

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
        # Remove incomplete file
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

Things to notice:

- `args.with` declares a full CLI interface with options, types, and help text —
  all as structured data in vertical layout
- `strand.pool` consumes URLs lazily with bounded concurrency
- `progress.show` tracks each download with bytes-level progress
