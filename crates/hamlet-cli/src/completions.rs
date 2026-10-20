//! Hand-rolled shell completion generation (SPEC §12.1; ADR-0042).
//!
//! Generated without external dependencies (`clap_complete`) to preserve
//! the locked runtime dependency allowlist (SPEC §3; CONVENTIONS §2).

use crate::client::ClientError;

/// Generate completion script for the given shell.
pub fn generate(shell: &str) -> Result<String, ClientError> {
    match shell.to_lowercase().as_str() {
        "bash" => Ok(BASH_COMPLETION.to_string()),
        "zsh" => Ok(ZSH_COMPLETION.to_string()),
        "fish" => Ok(FISH_COMPLETION.to_string()),
        other => Err(ClientError::Usage(format!(
            "unsupported shell '{other}'; supported: bash, zsh, fish"
        ))),
    }
}

const BASH_COMPLETION: &str = r#"# bash completion for hamlet
_hamlet() {
    local cur prev words cword
    _init_completion || return

    local commands="init daemon commission agent decree mail vault metrics completions help"
    local daemon_cmds="start run status stop"
    local comm_cmds="list new show close"
    local agent_cmds="list show hibernate wake steer"
    local decree_cmds="list show"
    local mail_cmds="list read send"
    local vault_cmds="list export"
    local metrics_cmds="show export"

    if [[ $cword -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
        return 0
    fi

    case "${words[1]}" in
        daemon)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$daemon_cmds" -- "$cur") )
            fi
            ;;
        commission)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$comm_cmds" -- "$cur") )
            fi
            ;;
        agent)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$agent_cmds" -- "$cur") )
            fi
            ;;
        decree)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$decree_cmds" -- "$cur") )
            fi
            ;;
        mail)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$mail_cmds" -- "$cur") )
            fi
            ;;
        vault)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$vault_cmds" -- "$cur") )
            fi
            ;;
        metrics)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$metrics_cmds" -- "$cur") )
            fi
            ;;
        completions)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") )
            fi
            ;;
    esac
}
complete -F _hamlet hamlet
"#;

const ZSH_COMPLETION: &str = r#"#compdef hamlet

_hamlet() {
    local -a commands
    commands=(
        'init:Bootstrap village tree and charter'
        'daemon:Daemon lifecycle control'
        'commission:Commission state machine'
        'agent:Agent stasis, census, and steering'
        'decree:Mayor decrees and human escalations'
        'mail:Courier mail messaging'
        'vault:Promoted artifact vault'
        'metrics:System and agent telemetry metrics'
        'completions:Generate shell completions'
    )

    _arguments -C \
        '--dir[Target village root directory]:directory:_files -/' \
        '--json[Strict JSON stdout output]' \
        '-q[Quiet]' \
        '-v[Verbose]' \
        '1: :->command' \
        '*:: :->args'

    case $state in
        command)
            _describe 'commands' commands
            ;;
        args)
            case $line[1] in
                daemon)
                    local -a daemon_cmds
                    daemon_cmds=('start:Start daemon' 'run:Run foreground governor' 'status:Heartbeat view' 'stop:Stop daemon')
                    _describe 'daemon commands' daemon_cmds
                    ;;
                commission)
                    local -a comm_cmds
                    comm_cmds=('list:List commissions' 'new:Create commission' 'show:Show commission' 'close:Close commission')
                    _describe 'commission commands' comm_cmds
                    ;;
                agent)
                    local -a agent_cmds
                    agent_cmds=('list:List census' 'show:Show agent' 'hibernate:Hibernate agent' 'wake:Wake patron' 'steer:Steer agent')
                    _describe 'agent commands' agent_cmds
                    ;;
                decree)
                    local -a decree_cmds
                    decree_cmds=('list:List decrees' 'show:Show decree')
                    _describe 'decree commands' decree_cmds
                    ;;
                mail)
                    local -a mail_cmds
                    mail_cmds=('list:List mail' 'read:Read mail' 'send:Send mail')
                    _describe 'mail commands' mail_cmds
                    ;;
                vault)
                    local -a vault_cmds
                    vault_cmds=('list:List vault deliverables' 'export:Export deliverable')
                    _describe 'vault commands' vault_cmds
                    ;;
                metrics)
                    local -a metrics_cmds
                    metrics_cmds=('show:Show metrics' 'export:Export metrics JSON')
                    _describe 'metrics commands' metrics_cmds
                    ;;
                completions)
                    local -a shells
                    shells=('bash:Bash' 'zsh:Zsh' 'fish:Fish')
                    _describe 'shells' shells
                    ;;
            esac
            ;;
    esac
}

_hamlet "$@"
"#;

const FISH_COMPLETION: &str = r#"# fish completion for hamlet

complete -c hamlet -f
complete -c hamlet -n "__fish_use_subcommand" -a init -d "Bootstrap village tree and charter"
complete -c hamlet -n "__fish_use_subcommand" -a daemon -d "Daemon lifecycle control"
complete -c hamlet -n "__fish_use_subcommand" -a commission -d "Commission state machine"
complete -c hamlet -n "__fish_use_subcommand" -a agent -d "Agent stasis, census, and steering"
complete -c hamlet -n "__fish_use_subcommand" -a decree -d "Mayor decrees and human escalations"
complete -c hamlet -n "__fish_use_subcommand" -a mail -d "Courier mail messaging"
complete -c hamlet -n "__fish_use_subcommand" -a vault -d "Promoted artifact vault"
complete -c hamlet -n "__fish_use_subcommand" -a metrics -d "System and agent telemetry metrics"
complete -c hamlet -n "__fish_use_subcommand" -a completions -d "Generate shell completions"

# daemon subcommands
complete -c hamlet -n "__fish_seen_subcommand_from daemon" -a start -d "Start daemon"
complete -c hamlet -n "__fish_seen_subcommand_from daemon" -a run -d "Run foreground daemon"
complete -c hamlet -n "__fish_seen_subcommand_from daemon" -a status -d "Check daemon status"
complete -c hamlet -n "__fish_seen_subcommand_from daemon" -a stop -d "Stop daemon"

# commission subcommands
complete -c hamlet -n "__fish_seen_subcommand_from commission" -a list -d "List commissions"
complete -c hamlet -n "__fish_seen_subcommand_from commission" -a new -d "New commission"
complete -c hamlet -n "__fish_seen_subcommand_from commission" -a show -d "Show commission"
complete -c hamlet -n "__fish_seen_subcommand_from commission" -a close -d "Close commission"

# agent subcommands
complete -c hamlet -n "__fish_seen_subcommand_from agent" -a list -d "List agents"
complete -c hamlet -n "__fish_seen_subcommand_from agent" -a show -d "Show agent"
complete -c hamlet -n "__fish_seen_subcommand_from agent" -a hibernate -d "Hibernate agent"
complete -c hamlet -n "__fish_seen_subcommand_from agent" -a wake -d "Wake patron"
complete -c hamlet -n "__fish_seen_subcommand_from agent" -a steer -d "Steer agent"

# completions shells
complete -c hamlet -n "__fish_seen_subcommand_from completions" -a "bash zsh fish"
"#;
