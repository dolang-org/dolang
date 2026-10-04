set nomore
let s:fixtures = json_decode(join(readfile(argv(0)), "\n"))
let s:syntax = argv(1)
let s:failures = []
for s:fixture in s:fixtures
  enew!
  unlet! b:current_syntax
  syntax clear
  call setline(1, s:fixture.lines)
  execute 'source ' . fnameescape(s:syntax)
  syntax sync fromstart
  for s:check in s:fixture.checks
    let s:line = s:fixture.lines[s:check.line - 1]
    let s:start = stridx(s:line, s:check.text, get(s:check, 'from', 0))
    if s:start < 0
      call add(s:failures, s:fixture.name . ': missing fixture text ' . s:check.text)
      continue
    endif
    for s:column in range(s:start + 1, s:start + strlen(s:check.text))
      let s:group = synIDattr(synID(s:check.line, s:column, 1), 'name')
      if has_key(s:check, 'vim') && index(s:check.vim, s:group) < 0
        call add(s:failures, printf('%s %d:%d %s: expected %s, got %s', s:fixture.name, s:check.line, s:column, s:check.text, string(s:check.vim), s:group))
      endif
      if index(get(s:check, 'notVim', []), s:group) >= 0
        call add(s:failures, printf('%s %d:%d %s: unexpected %s', s:fixture.name, s:check.line, s:column, s:check.text, s:group))
      endif
    endfor
  endfor
endfor
if !empty(s:failures)
  for s:failure in s:failures
    echomsg s:failure
  endfor
  cquit
endif
qa!
