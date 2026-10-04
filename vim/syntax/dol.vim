if exists("b:current_syntax")
  finish
endif

" Keywords decay to identifiers after field access and before a key colon.
syn match dolConstant "\%([A-Za-z0-9_.#]\)\@<!\<\%(false\|true\|nil\)\>\%(:\)\@!"
syn match dolKeyword "\%([A-Za-z0-9_.#]\)\@<!\<\%(break\|continue\|return\|do\|let\|def\|pub\|bind\|class\|field\|try\|catch\|finally\|throw\)\>\%(:\)\@!"
syn match dolConditional "\%([A-Za-z0-9_.#]\)\@<!\<\%(if\|else\|match\)\>\%(:\)\@!"
syn match dolRepeat "\%([A-Za-z0-9_.#]\)\@<!\<\%(for\|while\)\>\%(:\)\@!"
syn match dolInclude "\%([A-Za-z0-9_.#]\)\@<!\<import\>\%(:\)\@!"

syn match dolOperator "[-+/*<>=!%&|^~$]" contained
syn match dolOperator "\.\.\%([.]\)\@!" contained
syn match dolOperator "\s\zs=\ze\s"
syn match dolOperator "\s\zs\$\ze\(\s\|$\)"
syn match dolEllipsis "\.\.\." contained
syn match dolDelimiter "[,|]"
syn match dolColon ":"
syn match dolIdentifier "\<\%(true\>\|false\>\|nil\>\|do\>\)\@![A-Za-z_][A-Za-z0-9_]*" contained

syn cluster dolStrings contains=dolString,dolRawString,dolHereString,dolRawHereString
syn cluster dolExprList contains=dolOperator,dolNumber,dolConstant,dolIdentifier,@dolStrings,dolDelimiter,dolKeyword,dolConditional,dolRepeat,dolInclude,dolFullExpr,dolSymbol,dolDittoKey,dolKey,dolEllipsis,dolAnnotation,dolReturnType,dolLambda
syn cluster dolCompactExprList contains=@dolExprList

syn region dolFullExpr matchgroup=dolDelimiter start="(" end=")" contains=@dolExprList
syn region dolFullExpr matchgroup=dolDelimiter start="\[" end="]" contains=@dolExprList
syn region dolFullExpr matchgroup=dolDelimiter start="{" end="}" contains=@dolExprList
syn region dolCompactExpr matchgroup=dolSpecial start="\%(\$\|\.\.\.\)\ze\S" end="\ze\s\|$" contains=@dolCompactExprList

syn region dolDecorator matchgroup=dolSpecial start="#\[" end="\]" contains=@dolExprList
syn match dolComment "\(^\|\s\)\zs#\(\[\)\@!.*$" contains=@Spell
syn cluster dolFmtInterpList contains=dolFmtSpec,dolOperator,dolNumber,dolConstant,dolIdentifier,@dolStrings,dolDelimiter,dolFullExpr,dolSymbol,dolEllipsis

syn match dolStringInterp "\$[A-Za-z_][A-Za-z0-9_]*" contains=dolIdentifier,dolInterpMarker contained
syn match dolInterpMarker "\$" contained
syn region dolStringInterp matchgroup=dolSpecial start="\$(" end=")" contains=@dolExprList contained
" Formatted interpolation may contain nested expressions and strings.
syn region dolStringInterp matchgroup=dolSpecial start="\${#\?" end="}" contains=@dolFmtInterpList contained
syn match dolStringInterp "\$#\%([0-9]\+\|[A-Za-z_][A-Za-z0-9_]*\)" contains=dolIdentifier,dolInterpMarker contained
syn region dolFmtSpec matchgroup=dolColon start=":" end="}"me=s-1 contains=dolStringInterp contained

syn region dolString matchgroup=dolQuote start=+\%([bt]\)\?"+ end=+"+ skip=+\\\\\|\\"+ contains=dolEscape,dolStringInterp,@Spell
syn region dolRawString matchgroup=dolQuote start=+\%([A-Za-z0-9_]\)\@<!r\z(#*\)"+ end=+"\z1+ contains=@Spell
syn match dolEscape +\\\%([0nrt"\\$]\|x[0-9A-Fa-f]\{2}\|u{[0-9A-Fa-f]\%(_\?[0-9A-Fa-f]\)\{0,5}}\)+ contained

" Capture the first nonblank content line's indentation, not the introducer's.
" The part after \ze looks ahead across lines without colouring preceding code.
syn region dolHereString matchgroup=dolQuote start=+\%([A-Za-z0-9_|]\)\@<!t\?|\-\?\ze[ \t]*\%(#.*\)\?\n\%([ \t]*\n\)*\z([ \t]\+\)\S+ end=+^\%(\z1\)\@![ \t]*\ze\S+ contains=dolEscape,dolStringInterp,@Spell
syn region dolRawHereString matchgroup=dolQuote start=+\%([A-Za-z0-9_|]\)\@<!r|\-\?\ze[ \t]*\%(#.*\)\?\n\%([ \t]*\n\)*\z([ \t]\+\)\S+ end=+^\%(\z1\)\@![ \t]*\ze\S+ contains=@Spell

syn match dolNumber "\%(\%([A-Za-z0-9_.]\)\@<!\|\%(\.\.\)\@<=\)-\?\%(0[xX][0-9A-Fa-f]\%(_\?[0-9A-Fa-f]\)*\|0[oO][0-7]\%(_\?[0-7]\)*\|0[bB][01]\%(_\?[01]\)*\|\%([0-9]\%(_\?[0-9]\)*\%(\.\%([.A-DF-Za-df-z_(]\)\@!\%([0-9]\%(_\?[0-9]\)*\)\?\)\?\|\.[0-9]\%(_\?[0-9]\)*\)\%(e[+-]\?[0-9]\%(_\?[0-9]\)*\)\?\)\%([A-Za-z0-9_]\|\.[0-9]\)\@!"
syn match dolKey "[A-Za-z_][A-Za-z0-9_]*:" contains=dolColon
syn match dolDittoKey ":[A-Za-z_][A-Za-z0-9_]*" contains=dolColon
syn match dolListItem "^\s*-\(\s-\)*\s"

" Nested delimiters admit whitespace; a compact type ends at outer whitespace.
syn cluster dolTypes contains=dolTypeName,dolTypeMarker,dolTypeGroup,dolNumber,dolConstant,@dolStrings,dolSymbol,dolKey,dolComment
syn match dolTypeName "[A-Za-z_][A-Za-z0-9_]*" contained
syn match dolTypeMarker "->\|\.\.\.\|\*\*\|[?*@<>|=:,.]" contained
syn region dolTypeGroup extend matchgroup=dolTypeDelimiter start="(" end=")" contains=@dolTypes contained
syn region dolTypeGroup extend matchgroup=dolTypeDelimiter start="\[" end="\]" contains=@dolTypes contained
syn region dolTypeGroup extend matchgroup=dolTypeDelimiter start="{" end="}" contains=@dolTypes contained
syn match dolSymbol ":[A-Za-z_][A-Za-z0-9_]*:"
syn region dolAnnotation matchgroup=dolTypeMarker start="@\s*\ze\%(\%(def\|class\|let\|import\)\>\)\@!\S" end="\ze[ \t,)=\]}]\|$" contains=@dolTypes
syn region dolReturnType matchgroup=dolTypeMarker start="->\s\+\ze\S" end="\ze[ \t,)=\]}]\|$" contains=@dolTypes
syn region dolImplicit matchgroup=dolTypeMarker start="\%(^\|\s\)\zs[<>]\ze\S" end="\ze[ \t|]\|$" contains=@dolTypes contained
syn region dolBinders extend matchgroup=dolTypeDelimiter start="\%(\<\%(def\|class\)\>[ \t]\+[A-Za-z_][A-Za-z0-9_]*\)\@<=\[" end="\]" contains=@dolTypes contained
syn region dolSupertype matchgroup=dolColon start=":\s\+" end="$" contains=@dolTypes contained
syn match dolTypeOnly "@\ze\%(def\|class\|let\|import\)\>" nextgroup=dolDeclaration,dolTypeAlias,dolImport skipwhite
syn match dolDeclaredType "\%(\<class\>\s\+\)\@<=[A-Za-z_][A-Za-z0-9_]*" nextgroup=dolBinders contained
syn match dolDeclaredName "\%(\<def\>\s\+\)\@<=\%([A-Za-z_][A-Za-z0-9_]*\|([A-Za-z_][A-Za-z0-9_]*)\)" nextgroup=dolBinders contained
syn match dolRest "\.\.\.\|\*\*\|\*" contained
syn region dolDeclaration keepend matchgroup=dolKeyword start="\%([A-Za-z0-9_.#]\)\@<!\<\%(def\|class\)\>\%(:\)\@!" end="$" contains=dolKeyword,dolDeclaredName,dolDeclaredType,dolBinders,dolSupertype,dolAnnotation,dolReturnType,dolImplicit,dolRest,@dolStrings,dolNumber,dolConstant,dolFullExpr,dolDittoKey,dolComment
syn region dolTypeAlias keepend start="@let\s\+" end="$" contains=dolKeyword,dolTypeOnly,@dolTypes
syn match dolAliasKeyword "\<let\>" contained containedin=dolTypeAlias
syn match dolImportType "\%(\<import\s\+\|^\s*-\s\+\)\@<=@[A-Za-z_][A-Za-z0-9_]*\%([.][A-Za-z_][A-Za-z0-9_]*\)*" contains=dolTypeName,dolTypeMarker
syn region dolImport matchgroup=dolInclude start="\%([A-Za-z0-9_.#]\)\@<!\<import\>\%(:\)\@!" end="$" contains=dolInclude,dolImportType,dolIdentifier,dolColon,dolComment

syn cluster dolPatterns contains=dolPatternType,dolPatternGroup,dolIdentifier,dolRest,dolPatternBar,dolPatternDollar,dolAnnotation,@dolStrings,dolNumber,dolConstant,dolKey,dolDittoKey,dolSymbol,dolListItem,dolComment
syn match dolPatternType "[A-Za-z_][A-Za-z0-9_]*\%([.][A-Za-z_][A-Za-z0-9_]*\)*\ze\%((\|\s\+\$\s*$\)" contained
syn region dolPatternGroup matchgroup=dolDelimiter start="(" end=")" contains=@dolPatterns contained
syn match dolPatternBar "|" contained
syn match dolPatternDollar "\$\s*$" contained
syn region dolBinding matchgroup=dolKeyword start="\%([A-Za-z0-9_.#]\)\@<!\<\%(let\|for\)\>\s\+" end="\ze=\|$" contains=@dolPatterns
syn match dolRest "^[ \t]*\zs\*\*\?\ze[A-Za-z_]"
syn region dolVerticalImplicit matchgroup=dolTypeMarker start="^[ \t]*\zs[<>]\ze\S" end="\ze[ \t|]\|$" contains=@dolTypes
syn region dolLambda matchgroup=dolDelimiter start="\%(\<do\s\+\)\@<=|" end="|" contains=@dolPatterns,dolImplicit
syn region dolVerticalPattern matchgroup=dolKeyword start="^\z([ \t]*\)bind\>\%(:\)\@!" end="^\%(\z1[ \t]\+\)\@![ \t]*\ze\S" contains=@dolPatterns,dolPatternScrutinee
syn region dolPatternScrutinee start="\%(\<bind\>\)\@<=[ \t]\+" end="$" contains=@dolExprList contained

" A match arm owns its indented code body, keeping type tests out of calls.
syn region dolMatchBlock keepend start="\%(^[ \t]*\|=[ \t]*\)\@<=match\>\%(:\)\@!.*\ze\n\%([ \t]*\n\)*\z([ \t]\+\)\S" end="^\%(\z1\)\@![ \t]*\ze\S" contains=dolConditional,dolMatchArm
syn region dolMatchArm keepend start="^\z([ \t]\+\)\ze\S" end="^\ze\z1\S\|^\ze\%(\z1\)\@![ \t]*\S" contains=@dolPatterns,dolArmBody,dolArmCode contained
syn region dolArmCode start="$" end="\%$" contains=@dolCode contained
syn region dolArmBody matchgroup=dolKeyword start="\<\%(do\|if\)\>\%(:\)\@!" end="\%$" contains=@dolCode contained
syn cluster dolCode contains=dolKeyword,dolConditional,dolRepeat,dolInclude,dolConstant,dolOperator,dolDelimiter,dolColon,dolCompactExpr,dolFullExpr,dolDecorator,dolComment,@dolStrings,dolNumber,dolKey,dolDittoKey,dolSymbol,dolListItem,dolAnnotation,dolReturnType,dolDeclaration,dolTypeAlias,dolTypeOnly,dolImport,dolImportType,dolBinding,dolLambda,dolVerticalPattern,dolMatchBlock

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
hi def link dolRawString String
hi def link dolHereString String
hi def link dolRawHereString String
hi def link dolQuote String
hi def link dolStringInterp Special
hi def link dolInterpMarker Special
hi def link dolSpecial Special
hi def link dolEscape String
hi def link dolFmtSpec Special
hi def link dolNumber Number
hi def link dolSymbol Constant
hi def link dolDittoKey Normal
hi def link dolListItem Special
hi def link dolEllipsis Special
hi def link dolColon Delimiter
hi def link dolTypeName Type
hi def link dolPatternType Type
hi def link dolTypeMarker Special
hi def link dolTypeDelimiter Delimiter
hi def link dolTypeOnly PreProc
hi def link dolAliasKeyword Keyword
hi def link dolDeclaredName Function
hi def link dolDeclaredType Type
hi def link dolRest Special
hi def link dolPatternBar Delimiter
hi def link dolPatternDollar Special

syn sync fromstart
let b:current_syntax = "dol"
