# Performance

## Orçamento

A meta para reprodução local ociosa é até 50 MiB de RSS, até 1% de um núcleo em média
e zero underruns após aquecimento. Busca, download, importação e conversão são trabalho
ativo e ficam fora dessa meta, mas recebem limites para não competir agressivamente
com o player.

## Decisões de projeto

- CPAL conversa diretamente com ALSA/CoreAudio.
- Symphonia decodifica em processo, sem FFmpeg durante playback.
- Ring buffer atômico evita mutex e alocação no callback.
- Buffer de dois segundos prioriza estabilidade sob carga.
- A TUI redesenha no máximo uma vez por segundo quando nada muda.
- Download padrão usa um worker, uma conexão por fragmento e limite de 8 MiB/s.
- FFmpeg converte uma faixa por vez, com um thread e prioridade reduzida.
- Builds Cargo usam um job para preservar a responsividade da máquina.

## Benchmark

Use uma faixa Opus maior que aquecimento mais medição:

```bash
cargo build --release --locked
target/release/ezmusic benchmark musica.opus \
  --warmup-seconds 10 --measure-seconds 60
```

O comando zera underruns depois do aquecimento, mede CPU do processo com `getrusage`,
RSS no `/proc` no Linux ou `getrusage` no macOS e falha com código diferente de zero
quando ultrapassa o orçamento. O aquecimento aceita até 300 s e a medição, entre 1 s e
3.600 s. O comando também falha se a faixa terminar ou o backend emitir erro durante a
janela.

Se instalou essa mesma compilação no PATH, `ezmusic benchmark ...` é equivalente.

## Baseline conhecido

Em Linux Mint 22.3, com Opus 48 kHz, 3 s de aquecimento e 10 s de medição, a revisão
de 2026-07-21 registrou 14,19 MiB de RSS, 3,077% de um núcleo e zero underruns. Memória
e estabilidade passaram, mas CPU ainda excedeu a meta de 1%; portanto, o orçamento
completo permanece **não atingido**. Esse resultado curto é apenas uma linha de base,
não uma garantia sob carga nem evidência para macOS.

Para um teste representativo, comece a reprodução, estabilize o sistema e então rode
a carga real desejada em outros processos. Registre modelo de CPU, dispositivo de
áudio, sample rate, formato da faixa, RSS, CPU e underruns. Uma execução em ambiente
headless não prova performance de áudio.

## Interpretação

Underruns indicam que o decoder não alimentou o ring a tempo; primeiro verifique
dispositivo e carga de I/O. CPU alta com zero underruns aponta para codec/resampling ou
sample rate incomum. RSS alta deve ser comparada entre faixas: crescimento contínuo é
mais preocupante que um pico estável de inicialização.

O benchmark cobre o motor de áudio, não uma busca ou conversão simultânea. A TUI foi
desenhada para baixo custo, mas a validação final deve incluir uma sessão interativa na
máquina alvo com a carga concorrente desejada (IDE, containers e agentes).
