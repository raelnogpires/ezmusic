# Arquitetura

## Visão geral

EzMusic é um único pacote Rust. `src/main.rs` interpreta a CLI e abre configuração
e banco; `src/tui.rs` coordena interface e casos de uso. Os módulos não executam
strings em shell: argumentos externos são passados diretamente para
`std::process::Command`.

```text
TUI/CLI
  ├─ config.rs ─ diretórios e config.toml
  ├─ db.rs ───── SQLite: biblioteca, fila e playlists
  ├─ model.rs ─ tipos compartilhados de faixa, busca e eventos
  ├─ process.rs subprocessos limitados e encerramento de grupos
  ├─ source.rs ─ yt-dlp: busca e resolução de metadados
  ├─ tools.rs ── descoberta, download, hash e rollback
  ├─ storage.rs  verificação de espaço livre
  ├─ download.rs yt-dlp → cache → FFmpeg → .opus atômico
  └─ player.rs ─ Symphonia → ring buffer → CPAL/ALSA/CoreAudio
```

## Reprodução

O decoder roda em um thread comum e alimenta um ring buffer SPSC de dois segundos.
O callback de áudio apenas lê amostras atômicas, aplica volume e escreve no buffer do
dispositivo; ele não acessa disco, não espera mutex e não envia eventos bloqueantes.
Eventos usam um canal limitado com `try_send`. A reprodução valida sample rate e
número de canais e pré-carrega 100 ms antes de iniciar o stream.

Seek é comunicado por atômico. O decoder limpa o ring, reposiciona o demuxer e
recomeça o resampler linear. Encerrar ou trocar de faixa sinaliza o decoder e aguarda
seu término antes de liberar o stream.

## Busca e download

`YouTubeProvider` aceita somente HTTP(S) nos hosts YouTube. yt-dlp ignora arquivos de
configuração externos, tem retries e timeout, retorna no máximo 500 itens e sua saída
JSON é limitada a 16 MiB. Isso impede playlists ou processos defeituosos de crescerem
memória indefinidamente.

Uma consulta textual vira `ytsearchN:consulta`; uma URL é resolvida diretamente.
Resultados do tipo álbum/playlist podem ser abertos como uma página ou expandidos para
download integral. A busca textual continua usando metadados planos; ao abrir ou preparar
uma faixa/resultado desconhecido, uma segunda extração sem `--flat-playlist` inspeciona os
metadados completos. URLs diretas do YouTube e YouTube Music seguem o mesmo caminho.

Um vídeo isolado só vira álbum quando possui pelo menos dois capítulos estruturados
válidos ou, na ausência deles, um índice conservador de timestamps no início das linhas da
descrição. O parser aceita `M:SS`, `MM:SS` e `H:MM:SS`, exige o primeiro marco em zero,
ordem estritamente crescente, limites finitos dentro da duração e segmentos positivos.
Dados inválidos, ambíguos ou com uma única entrada mantêm o comportamento de faixa única.
Marcadores no título, como “album”, “LP” ou “OST”, são apenas sinais para inspeção e nunca
classificam o conteúdo sozinhos. Playlists comuns também não viram álbuns implicitamente.

Antes de entrar na fila, resultados são deduplicados por provider e ID, preservando a
ordem da coleção, e toda a operação continua limitada a 500 faixas.

`DownloadService` possui fila limitada e workers fixos. O padrão é um worker; mesmo
com mais downloads de rede, um mutex permite apenas uma conversão. Cada subprocesso
entra em seu próprio grupo, recebe prioridade `nice +10`, prazo máximo e é encerrado
com seus descendentes em cancelamento ou shutdown. O serviço aguarda todos os workers
no `Drop`.

O arquivo convertido nasce como `.opus.part` e só é renomeado para o destino após o
FFmpeg terminar com sucesso. Para um vídeo com capítulos, yt-dlp baixa uma única fonte e
o FFmpeg decodifica cortes precisos, sem stream copy, em arquivos Opus independentes com
título, artista, álbum e posição. O evento concluído carrega todas as faixas do lote; a
TUI só persiste o álbum depois que todas estão prontas. Em cancelamento ou erro, o `.part`
atual é removido, arquivos anteriores não são apagados e o cache de entrada é preservado
para retomada. O cache é removido após sucesso. Cache e biblioteca precisam ter pelo menos
2 GiB livres antes de cada faixa.

## Persistência

SQLite usa WAL, foreign keys e `synchronous=NORMAL`. `albums` identifica cada álbum por
provider/source ID; `album_tracks` relaciona suas faixas em ordem, mantendo `tracks.album`
para metadados e compatibilidade. A migração é aditiva e idempotente. O upsert do álbum,
das faixas e da associação ordenada ocorre em uma transação, evitando coleções parciais.

A fila é substituída dentro de transação. Remover uma faixa de playlist compacta suas
posições transacionalmente sem apagar a faixa ou alterar a fila ativa, que funciona como
snapshot. Importações não seguem symlinks, aceitam até 100 mil arquivos, fazem as
alterações em transação e marcam como indisponíveis apenas arquivos pertencentes à
raiz reindexada. Arquivos importados permanecem no local original; os formatos indexáveis
são Opus/Ogg, MP3, FLAC, AAC/M4A, WAV e WebM.

## Interface

A TUI processa cada evento de teclado uma única vez, a cada janela de até 100 ms, mas
só redesenha após mudança ou uma vez por segundo para atualizar a posição. O layout
é responsivo: terminais largos recebem um painel de sessão; os compactos preservam a
lista e o deck do player. Uma guarda RAII restaura cursor, tela alternativa e modo raw
mesmo quando uma operação retorna erro. A navegação de coleções conserva no máximo oito
páginas de histórico com cursor e seleção. Biblioteca e playlists possuem uma lista
principal e uma tela de detalhes; entrar abre sem tocar, `A` toca desde o início e voltar
restaura a seleção anterior. Ações longas rodam fora do thread da interface.
