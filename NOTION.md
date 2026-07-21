# EzMusic — música sem disputar recursos com o seu trabalho

O EzMusic é um aplicativo de terminal para Linux e macOS que permite buscar, baixar,
organizar e ouvir músicas em um só lugar. Ele nasceu com uma prioridade clara: continuar
tocando com estabilidade mesmo quando o computador está ocupado com IDEs, containers,
bancos de dados e vários agentes de IA.

## O problema

Ouvir música durante o trabalho normalmente significa manter um navegador ou aplicativo
pesado aberto. Essas soluções consomem memória, CPU e conexão continuamente — justamente
os recursos necessários para compilar projetos, executar Docker e trabalhar com IA.
Ferramentas separadas para download, conversão, organização e reprodução também tornam o
fluxo mais trabalhoso do que deveria ser.

## A solução

O EzMusic reúne esse fluxo em uma TUI, uma interface visual que funciona dentro do
terminal e pode ser controlada inteiramente pelo teclado. O usuário pode pesquisar uma
faixa, colar uma URL pública do YouTube ou YouTube Music, escolher alguns resultados ou
baixar um álbum ou playlist completos. Depois disso, as músicas ficam em uma biblioteca
local e podem ser reproduzidas sem navegador, yt-dlp ou FFmpeg rodando em segundo plano.

## O que já é possível fazer

- Buscar artistas e faixas ou abrir URLs públicas.
- Baixar uma faixa, uma seleção ou uma coleção completa.
- Converter os downloads para Opus e organizá-los em uma biblioteca local.
- Importar pastas existentes sem mover ou duplicar os arquivos.
- Criar playlists e manter uma fila de reprodução persistente.
- Pausar, retomar, parar, avançar, voltar e fazer seek.
- Controlar volume, shuffle e modos de repetição pelo teclado.
- Diagnosticar áudio, configuração e ferramentas com `ezmusic doctor`.

## Como a solução foi construída

A busca e a leitura de coleções usam o yt-dlp. Cada download passa por limites de rede,
tempo, tamanho e espaço em disco. O FFmpeg converte somente uma faixa por vez e usa um
thread, reduzindo a disputa com outras aplicações. Arquivos incompletos nunca aparecem
como músicas prontas na biblioteca.

Durante a reprodução, o caminho é totalmente nativo. O Symphonia decodifica o arquivo
local, um buffer dedicado absorve variações de carga e o CPAL envia o áudio diretamente
para ALSA no Linux ou CoreAudio no macOS. O trecho mais sensível do player não acessa o
disco, não espera locks e não faz alocações.

## Tecnologia

O projeto é escrito em **Rust** e usa **Ratatui/Crossterm** para a interface de terminal,
**CPAL** e **Symphonia** para áudio nativo, **SQLite/Rusqlite** para biblioteca, fila e
playlists, além de **yt-dlp** e **FFmpeg** para integração com fontes públicas e conversão
para Opus. O binário é otimizado para release, e o projeto possui testes automatizados,
Clippy, rustfmt, auditoria de dependências e CI para Linux x86_64 e macOS ARM64.

## Performance como requisito de produto

A meta de reprodução local é permanecer abaixo de 50 MiB de memória, consumir no máximo
1% de um núcleo e não sofrer interrupções de áudio após o aquecimento. No baseline atual,
o player registrou 14,19 MiB de memória e zero underruns. O consumo médio de CPU foi de
3,077%, portanto a meta de CPU ainda é um trabalho em andamento e permanece visível, em
vez de ser tratada como concluída.

## Segurança e responsabilidade

O EzMusic não recebe cookies, credenciais ou tokens, não oferece login e não tenta
contornar DRM. Ferramentas auxiliares baixadas pelo aplicativo têm tamanho e SHA-256
verificados antes da ativação. O projeto deve ser usado somente para obter conteúdo
público que o usuário tenha autorização para baixar.

## Estado atual

O projeto está em fase de MVP funcional. Linux x86_64 é a plataforma principal, e
macOS ARM64 já faz parte do CI, embora a distribuição pública para macOS ainda dependa
de assinatura e notarização. O próximo grande objetivo é reduzir o consumo de CPU e
validar sessões longas de reprodução sob cargas reais de desenvolvimento.
