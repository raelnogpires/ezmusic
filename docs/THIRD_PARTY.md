# Componentes de terceiros

O código próprio do EzMusic usa MIT, conforme `LICENSE`. Dependências Rust listadas em
`Cargo.lock` mantêm suas próprias licenças; o lockfile fixa versões, mas não incorpora
seus textos de licença ao binário automaticamente. Uma distribuição pública deve gerar
e revisar um inventário de licenças antes do release.

yt-dlp e FFmpeg não fazem parte do repositório nem do binário EzMusic. Eles são
descobertos no `PATH` ou baixados para o diretório de dados do usuário:

- O [executável oficial do yt-dlp](https://github.com/yt-dlp/yt-dlp) inclui componentes
  sob múltiplas licenças e publica `THIRD_PARTY_LICENSES.txt` na release.
- Os binários de
  [`eugeneware/ffmpeg-static`](https://github.com/eugeneware/ffmpeg-static) são
  distribuídos separadamente, e o repositório declara GPL-3.0-or-later. A configuração
  e a licença efetiva de cada build devem ser consultadas com `ffmpeg -version` e na
  origem indicada pelo fornecedor.
- SQLite e libopus usados pelo app são compilados por crates Rust e continuam sujeitos
  às licenças dos respectivos projetos e bindings.

Não copie ferramentas gerenciadas para um artefato do EzMusic sem revisar atribuições,
licenças e termos de redistribuição da versão exata. O CI atual publica somente o
executável Rust; ferramentas são obtidas pelo usuário em runtime.
