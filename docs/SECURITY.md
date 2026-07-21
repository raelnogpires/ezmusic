# Segurança

## Fronteiras de confiança

Consultas e URLs são dados não confiáveis. Elas nunca formam comandos de shell; cada
argumento é enviado separadamente e `--` encerra opções antes de valores externos.
Somente URLs HTTP(S) de `youtube.com`, seus subdomínios e `youtu.be` são aceitas pelo
provider atual. Configurações pessoais do yt-dlp são ignoradas.

Metadados externos têm limites indiretos pelo JSON máximo de 16 MiB e pelo teto de 500
itens. Consultas aceitam 512 caracteres e caminhos de importação, 4.096. Identificadores
usados em nomes de arquivo aceitam apenas ASCII alfanumérico, `-` e `_`. A publicação
por rename impede que uma conversão parcial pareça completa.

## Ferramentas gerenciadas

Metadados de release e binários vêm por HTTPS do GitHub. O tamanho declarado e o
tamanho recebido são verificados, downloads possuem limites por ferramenta e o
SHA-256 publicado pelo GitHub precisa coincidir. O binário passa por smoke test antes
da ativação. A troca é atômica e conserva uma versão anterior. Os tetos são 64 MiB
para yt-dlp e 128 MiB para FFmpeg.

Ferramentas encontradas no `PATH` só são aceitas se forem executáveis e compatíveis:
yt-dlp respeita uma versão mínima e FFmpeg precisa anunciar `libopus`. `tools update`
é explícito; nenhuma manutenção de rede ocorre ao abrir o player.

## Isolamento de recursos

Subprocessos têm grupo próprio e timeout; download e conversão também recebem prioridade
reduzida. Cancelamento mata o grupo, e o encerramento do serviço aguarda os workers.
Downloads individuais não podem exceder 1 GiB e só começam com pelo menos 2 GiB livres
no cache e na biblioteca. Respostas HTTP, stdout e stderr têm limites de memória. No
Linux, os subprocessos também recebem um sinal de morte do pai para evitar ferramentas
de mídia órfãs ao sair do TUI.

## Fora de escopo

EzMusic não armazena credenciais, cookies ou tokens, não oferece login ou telemetria e
não contorna DRM. A verificação SHA-256 garante integridade em relação ao metadado do
GitHub; ela não substitui auditoria do código dos binários terceiros. Relate falhas sem
incluir conteúdo baixado, caminhos privados ou dados pessoais.
