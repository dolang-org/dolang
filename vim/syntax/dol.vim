if exists("b:current_syntax")
  finish
endif

syn keyword dolConstant false true nil
syn keyword dolKeyword break continue return do let def pub bind
syn keyword dolConditional if else
syn keyword dolRepeat for while
syn keyword dolInclude import

syn match dolOperator "[+-/*<>=!%&|\$]" contained
syn match dolOperator "\s\zs=\ze\s"
syn match dolOperator "\s\zs\$\ze\(\s\|$\)"
syn match dolEllipsis "\.\.\." contained

syn match dolDelimiter "[,|]"
syn match dolColon ":"

syn match dolIdentifier "[A-Za-z_][A-Za-z0-9_]*" contained

syn cluster dolExprList contains=dolOperator,dolNumber,dolConstant,dolIdentifier,dolString,dolDelimiter,dolKeyword,dolFullExpr,dolSymbol,dolDittoKey,dolKey,dolEllipsis
syn cluster dolCompactExprList contains=dolOperator,dolNumber,dolConstant,dolIdentifier,dolString,dolDelimiter,dolKeyword,dolFullExpr,dolListItem

syn region dolFullExpr matchgroup=dolDelimiter start="(" end=")" contains=@dolExprList
syn region dolFullExpr matchgroup=dolDelimiter start="\[" end="]" contains=@dolExprList
syn region dolFullExpr matchgroup=dolDelimiter start="{" end="}" contains=@dolExprList
syn region dolCompactExpr matchgroup=dolSpecial start="\zs\(\$\|\.\.\.\)\ze[^ \t]" end="\zs\ze[^A-Za-z0-9_]" contains=@dolCompactExprList

syn region dolDecorator matchgroup=dolSpecial start="#\[" end="\]" contains=@dolExprList
syn match dolComment "\(^\|\s\)\zs#\(\[\)\@!.*\ze$" contains=@Spell

syn region dolStringInterp matchgroup=dolSpecial start="\$" end="\zs\ze[^A-Za-z0-9_]" contains=doIdentifier contained
syn region dolStringInterp matchgroup=dolSpecial start="\$(" end=")" contains=@dolExprList contained
syn region dolString matchgroup=dolQuote start=+"+ end=+"+ skip="\\\\\|\\\"" contains=dolStringInterp,@Spell

syn match dolEscape +\\[nrt"\\\$]+ contained

syn match dolNumber "\%([A-Za-z0-9_]\@<!\)\zs-\?\(\d\+\|\d*\.\d\+\|\d\+\.\d*\)\(e[+-]\?\d\+\)\?\%([A-Za-z0-9_]\@!\)"

syn match dolKey "[A-Za-z_][A-Za-z0-9_]*:" contains=dolColon
syn match dolDittoKey ":[A-Za-z_][A-Za-z0-9_]*" contains=dolColon
syn match dolSymbol ":[A-Za-z_][A-Za-z0-9_]*:"

syn match dolListItem "^\s*-\(\s-\)*\s"


hi def link dolKeyword Keyword
hi def link dolConditional Conditional
hi def link dolConstant Constant
hi def link dolDelimiter Delimiter
hi def link dolIdentifier Normal
hi def link dolKey Label
hi def link dolRepeat Repeat
hi def link dolOperator Operator
hi def link dolInclude Include
hi def link dolComment Comment
hi def link dolDecorator PreProc
hi def link dolString String
hi def link dolQuote String
hi def link dolSpecial Special
hi def link dolEscape String
hi def link dolNumber Number
hi def link dolSymbol Constant
hi def link dolDittoKey Normal
hi def link dolListItem Special
hi def link dolEllipsis Special
hi def link dolColon Delimiter

let b:current_syntax = "dol"
