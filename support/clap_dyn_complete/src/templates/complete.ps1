# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

Register-ArgumentCompleter -Native -CommandName '__COMMAND_NAME__' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    if ($null -ne $env:DEBUG_COMPLETIONS) {
        Write-Output '' > 'debug.txt'
    }

    function Write-DebugCompletions {
        if ($null -ne $env:DEBUG_COMPLETIONS) {
            Write-Output $args >> 'debug.txt'
        }
    }

    $stdOutTempFile = "$env:TEMP\$((New-Guid).Guid)"
    $stdErrTempFile = "$env:TEMP\$((New-Guid).Guid)"

    $completion_subcommand = "__COMPLETION_SUBCOMMAND__" -Split " "

    $startProcessParams = @{
        FilePath               = $commandAst.CommandElements[0].Value
        ArgumentList           = $completion_subcommand + ("--position", $cursorPosition, "--raw", "`"$($commandAst.ToString())`"", $commandAst.ToString())
        RedirectStandardError  = $stdErrTempFile
        RedirectStandardOutput = $stdOutTempFile
        PassThru               = $true;
        NoNewWindow            = $true;
    }

    $cmd = Start-Process @startProcessParams
    $cmd.WaitForExit()

    $cmdOutput = Get-Content -Path $stdOutTempFile -Raw
    $cmdError = Get-Content -Path $stdErrTempFile -Raw

    Write-DebugCompletions "out:`n$cmdOutput`n--"
    Write-DebugCompletions "err:`n$cmdError`n--"

    $cmdOutput.Trim() -Split "`n" | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }
}

# To enable completions for this local session, run:
#
#     . { Invoke-Expression $($(__COMMAND_NAME__ completions powershell) -Join "`n") }
#
# To permanently install these completions, copy this code into your powershell $PROFILE
#
# Bonus suggestion: consider changing the default `Tab` behavior to use `MenuComplete`:
#
#     Set-PSReadlineKeyHandler -Chord Tab -Function MenuComplete
