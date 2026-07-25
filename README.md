# EzMusic

EzMusic é um downloader e player de música para terminal, priorizando Linux e
macOS ARM64. A reprodução é nativa: não mantém navegador, yt-dlp ou FFmpeg ativos
quando apenas toca um arquivo local.

## Estado do projeto

O MVP oferece busca textual no YouTube, resolução de URLs públicas do YouTube e
YouTube Music, seleção de faixas ou coleções, download completo de álbuns/playlists,
conversão Opus, biblioteca SQLite, importação de pastas, fila, playlists, sugestões
baseadas na biblioteca e player com
seek, volume, shuffle e repeat.

Limites de segurança deliberados:

- um download paralelo por padrão, configurável entre 1 e 4;
- uma conversão FFmpeg por vez, com um thread;
- até 500 faixas por operação e 512 downloads pendentes;
- entrada de mídia de até 1 GiB;
- pelo menos 2 GiB livres antes de iniciar uma faixa;
- download limitado a 8 MiB/s e processos de download/conversão com baixa prioridade;
- nenhuma atualização ou download automático ao abrir o player.

Baixe somente conteúdo público para o qual você tenha autorização. O projeto não
implementa cookies, login, DRM ou contorno de restrições.

## Plataformas e pré-requisitos

O código e o CI cobrem Linux x86_64 e macOS ARM64 (Apple Silicon). Rust 1.89 está
fixado em `rust-toolchain.toml`. No Linux Mint/Ubuntu, instale as bibliotecas de build
e de áudio:

```bash
sudo apt-get update
sudo apt-get install -y pkg-config libasound2-dev cmake
```

No macOS, instale as Command Line Tools do Xcode e o CMake. Se `cargo` não estiver
visível depois de instalar o Rust com rustup, recarregue o ambiente com
`source "$HOME/.cargo/env"` e abra um novo terminal.

## Build e instalação do comando

Os builds locais usam apenas um job por padrão, definido em `.cargo/config.toml`,
para manter o desktop responsivo:

```bash
cargo check --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

O primeiro build é demorado porque compila SQLite e Opus. `target/` pode ultrapassar
1 GiB; `cargo clean` recupera esse espaço, mas exige recompilar tudo.

Para executar `ezmusic` a partir de qualquer diretório, copie o release para o PATH
do usuário:

```bash
mkdir -p "$HOME/.local/bin"
install -m 0755 target/release/ezmusic "$HOME/.local/bin/ezmusic"
command -v ezmusic
```

Se o último comando não encontrar o binário, adicione
`export PATH="$HOME/.local/bin:$PATH"` ao `~/.zshrc` e abra outro terminal. Repita o
`install` após compilar uma versão nova; a instalação é uma cópia, não um symlink.

## Uso

```bash
ezmusic                 # abre a TUI; `ezmusic tui` é equivalente
ezmusic doctor          # inspeciona config, banco, áudio e ferramentas
ezmusic import /caminho/para/musicas  # indexa sem copiar arquivos
ezmusic tools status    # mostra as ferramentas utilizáveis
ezmusic tools update    # baixa/atualiza ferramentas explicitamente
ezmusic benchmark song.opus
```

Durante desenvolvimento, substitua `ezmusic` por `cargo run --release --locked --`.

## Controles da TUI

- Globais: `1`–`6` abrem Busca, Biblioteca, Fila, Downloads, Playlists e Sugestões; `j/k` navegam,
  `/` busca, `I` importa, `?` mostra ajuda e `q`/`Ctrl-C` saem.
- Player: `Space`/`p` pausa ou retoma, `z` para e libera o stream, `b/n` volta ou
  avança, setas fazem seek de 5 s, `+/-` alteram volume e `s/r` alternam shuffle/repeat.
- Busca: `x` ou `Space` marca, `a` marca tudo, `d` baixa a seleção, `Enter` abre o
  resultado e `A` baixa o álbum/playlist completo. `Esc`/`Backspace` volta da coleção.
- Sugestões: ao abrir a aba `6`, o app consulta até três artistas distintos da biblioteca
  (ou o nome do arquivo quando o artista importado é desconhecido), mostra até 20 faixas
  novas e não acessa a rede novamente até `R`. Use `x`/`Space`, `a` e `d` para selecionar
  e baixar; resultados já presentes na biblioteca são removidos.
- Biblioteca: a lista principal mostra álbuns e faixas avulsas. `Enter` abre um álbum ou
  toca uma faixa avulsa; dentro do álbum, `Enter` inicia a fila na faixa selecionada,
  `A` toca desde o início e `P` adiciona a faixa a uma playlist. `/` filtra por álbum,
  artista ou faixa, e `Esc`/`Backspace` volta preservando a seleção.
- Fila: `Enter` toca; sobre a faixa ativa, pausa ou retoma sem reiniciar. `Delete`/`x`
  remove a faixa selecionada da fila.
- Playlists: `Enter` abre os detalhes; ali, `Enter` inicia pela faixa selecionada, `A`
  toca desde o início e `Delete`/`x` remove apenas a associação com a playlist.
  `Esc`/`Backspace` volta à lista. A fila que já está tocando não muda ao editar.
- Downloads: `c` cancela o item selecionado.

Para baixar uma coleção, cole a URL pública do álbum ou playlist na busca, abra o
resultado quando necessário e pressione `A`. O app resolve no máximo 500 faixas e
mantém os mesmos limites de fila, rede, disco e conversão por faixa.

Um vídeo único com capítulos confiáveis pode virar um álbum: o arquivo-fonte é baixado
uma vez e cada capítulo é convertido em uma faixa Opus independente. Na ausência de
capítulos estruturados, o app aceita apenas um índice conservador de timestamps na
descrição. Títulos como “Full Album” apenas motivam a inspeção; sem ao menos duas marcas
válidas, crescentes e dentro da duração, o vídeo continua sendo uma faixa única.

No primeiro início, `Enter` aceita o aviso de download responsável; `q` ou `Esc` sai
sem aceitar.

Na primeira operação online, o app usa uma instalação compatível encontrada no
`PATH` ou baixa um binário gerenciado. Binários gerenciados têm tamanho limitado,
SHA-256 verificado e só são ativados após smoke test. `tools update` é sempre
explícito e preserva uma versão anterior para rollback.

## Dados e configuração

No Linux, são usados os diretórios XDG; no macOS, os diretórios equivalentes de
Application Support. No Linux padrão, a configuração fica em
`~/.config/ezmusic/config.toml`, o banco em
`~/.local/share/ezmusic/library.sqlite3` e as ferramentas em
`~/.local/share/ezmusic/tools/`. A biblioteca usa a pasta de Música localizada pelo
sistema (`$XDG_MUSIC_DIR/EzMusic`, por exemplo `~/Músicas/EzMusic`); se ela não puder
ser descoberta, o fallback é `./Music/EzMusic`. `ezmusic doctor` mostra os caminhos
efetivos.

Campos configuráveis:

| Campo | Padrão | Regra |
| --- | --- | --- |
| `library_path` | pasta de Música + `EzMusic` | destino dos downloads |
| `import_roots` | vazio | pastas indexadas sem copiar |
| `audio_device` | saída padrão | nome exato do dispositivo |
| `max_parallel_downloads` | `1` | intervalo `1..=4` |
| `opus_bitrate_kbps` | `160` | intervalo `64..=320` |
| `accepted_download_notice` | `false` | aceite do aviso legal |

A importação é recursiva, não segue symlinks e reconhece `.opus`, `.ogg`, `.oga`,
`.mp3`, `.flac`, `.aac`, `.m4a`, `.wav` e `.webm`. Os arquivos permanecem em seus
locais originais.

## Documentação

- [Apresentação pública do projeto](NOTION.md)
- [Guia de contribuição](AGENTS.md)
- [Arquitetura](docs/ARCHITECTURE.md)
- [Operação e recuperação](docs/OPERATIONS.md)
- [Performance](docs/PERFORMANCE.md)
- [Segurança](docs/SECURITY.md)
- [Componentes de terceiros](docs/THIRD_PARTY.md)
- [Relatório da revisão](docs/REVIEW.md)

O CI audita dependências com RustSec, formata, analisa, testa e compila Linux x86_64
e macOS ARM64. Apenas o artefato Linux é publicado enquanto o binário macOS não tiver
assinatura e notarização.
