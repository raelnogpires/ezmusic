# Operação e recuperação

## Diagnóstico seguro

Comece por `ezmusic doctor`. Ele abre configuração e banco, garante que o diretório da
biblioteca exista, enumera áudio com timeout de dois segundos e consulta versões de
ferramentas. O comando não pesquisa, baixa mídia nem atualiza binários. Ele não grava um
arquivo de teste dentro de uma biblioteca que já existe.

`ezmusic tools status` executa apenas os smoke tests locais. `ezmusic tools update`
acessa a rede e aceita no máximo 64 MiB para yt-dlp e 128 MiB para FFmpeg; execute-o
somente quando desejar manutenção. Abrir a TUI nunca inicia essa atualização.

## Primeiro uso

Uma busca exige yt-dlp compatível. Versões anteriores a `2026.01.01` são recusadas,
pois versões antigas frequentemente deixam de funcionar com o YouTube. O app prefere
uma ferramenta gerenciada válida, depois procura no `PATH` e só então baixa a release
mais recente. A primeira conversão segue a mesma regra para FFmpeg e exige o encoder
`libopus`.

## Fluxos principais

Abra a TUI com `ezmusic`. Pressione `/`, informe um termo ou uma URL pública do
YouTube e confirme com `Enter`. Na busca, `x` marca faixas e `d` baixa a seleção;
`A` resolve e baixa a coleção inteira. `Esc` ou `Backspace` retorna da coleção aberta.
Na tela 4, `c` cancela o download selecionado.

Use `I` na TUI ou `ezmusic import /caminho/para/musicas` para indexar áudio local sem
copiá-lo. A varredura é recursiva, não segue symlinks e aceita Opus/Ogg, MP3, FLAC,
AAC/M4A, WAV e WebM, com limite de 100 mil arquivos por raiz.

## Arquivos e recuperação

- Configuração Linux: `~/.config/ezmusic/config.toml`.
- Banco Linux: `~/.local/share/ezmusic/library.sqlite3`.
- Ferramentas Linux: `~/.local/share/ezmusic/tools/`.
- Cache Linux: `~/.cache/ezmusic/downloads/`.
- Biblioteca padrão: pasta de Música localizada pelo sistema + `EzMusic/`.

Os caminhos acima respeitam variáveis XDG e podem mudar. No macOS, ficam nos
diretórios equivalentes de Application Support. Use `ezmusic doctor` como fonte dos
caminhos efetivos de configuração, banco e biblioteca.

Arquivos `.part` na biblioteca nunca são indexados como faixas completas. Diretórios
no cache podem ser removidos quando o app estiver fechado; isso perde apenas a retomada
de downloads incompletos. O arquivo `*.previous` em `tools/` é o rollback da última
ferramenta substituída e não deve ser apagado durante uma atualização.

Antes de mexer no banco, feche o EzMusic e copie `library.sqlite3` junto com os arquivos
`-wal` e `-shm`, se existirem. Excluir o banco reconstrói um catálogo vazio; as músicas
no disco não são apagadas e podem ser reimportadas.

O executável instalado em `~/.local/bin/ezmusic` é uma cópia. Depois de um novo
`cargo build --release --locked`, publique a versão novamente:

```bash
install -m 0755 target/release/ezmusic "$HOME/.local/bin/ezmusic"
```

## Build sem travar o desktop

O repositório fixa `jobs = 1`. Não aumente `CARGO_BUILD_JOBS` enquanto Docker, IDEs ou
agentes estiverem pressionando memória. Verifique espaço com `du -sh target` e
`df -h .`. Para recuperar artefatos gerados:

```bash
cargo clean
```

Esse comando remove somente `target/`, mas a próxima compilação será completa. Para
diagnóstico incremental e barato, prefira `cargo check --locked`; deixe o build release
para o final.

## Falhas comuns

- `alsa-sys`: instale `pkg-config` e `libasound2-dev`.
- `opusic-sys`/CMake: instale `cmake` e confirme `cmake --version`.
- `cargo: command not found`: rode `source "$HOME/.cargo/env"`; se persistir, instale
  Rust 1.89 via rustup e reabra o zsh.
- Sem áudio: confirme o PipeWire/PulseAudio da sessão e rode `ezmusic doctor`.
- Busca incompatível: rode `ezmusic tools update`; não use o yt-dlp antigo da distro.
- `Unsupported url scheme: "ytmusicsearch..."`: o binário está desatualizado; compile
  novamente e reinstale `~/.local/bin/ezmusic`. A versão atual usa `ytsearchN:consulta`.
- Conversão sem `libopus`: instale um FFmpeg compatível ou atualize a ferramenta
  gerenciada.
- Terminal visualmente corrompido após kill externo: execute `reset` no shell.
