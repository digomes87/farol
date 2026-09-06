# Arquitetura

Este documento registra as decisões de projeto do `farol` e o motivo de cada
uma. O README mostra o que o programa faz; aqui está por que ele faz assim.

## Visão geral

O sistema é um pipeline. Nenhum estágio conhece o anterior, e todos são
independentes o bastante para serem testados sozinhos.

```text
indexação:  arquivos → analyzer → shards → merge → índice → disco
consulta:   string   → parser   → cláusulas → filtro → BM25 → snippet
```

O crate `farol-core` contém o pipeline inteiro e não depende do CLI. O crate
`farol-cli` só cuida de argumentos, formatação e cores.

## Decisões

### 1. O analisador é o contrato entre indexação e consulta

Se a indexação produz `coracao` e a consulta produz `coracoes`, a busca não
encontra nada — e o erro é silencioso, que é a pior categoria de erro em busca.
Por isso existe um único tipo `Analyzer`, usado nos dois lados, e o índice
guarda uma referência a ele.

O stemming roda em duas passadas. A primeira colapsa a flexão
(`migracoes → migracao`), a segunda a derivação (`migracao → migr`). Com uma
passada só, plural e singular caem em termos diferentes: o plural sai como
`migracao` e o singular como `migr`. Um teste de idempotência (`stem(stem(x)) ==
stem(x)`) protege essa propriedade.

### 2. Posições são guardadas por posting

Custam espaço, e são o que separa "as duas palavras estão neste documento" de
"as duas palavras estão lado a lado". Sem elas, frase exata é impossível.

As posições são atribuídas **antes** da remoção de stopwords. Assim, em
`"canto de sereia"`, os termos `canto` e `sereia` ficam a duas posições de
distância tanto no documento quanto na consulta, e a frase continua casando
mesmo com o `de` descartado dos dois lados.

### 3. IDF na forma probabilística suavizada

```text
idf = ln(1 + (N - df + 0.5) / (df + 0.5))
```

O `+1` dentro do logaritmo não é decoração. A fórmula probabilística crua fica
negativa para termos presentes em mais da metade da coleção, e um IDF negativo
faz um termo comum *subtrair* pontos de um documento que o contém. Com a
suavização, o pior caso é contribuir zero.

### 4. Candidatos saem da interseção, não da união

Quando a consulta tem cláusula obrigatória, o conjunto de candidatos é a
interseção das listas correspondentes. Pontuar só os sobreviventes é
substancialmente mais barato do que pontuar a união e descartar depois — o
benchmark mostra 10 µs contra 43 µs para a mesma consulta.

O casamento de frase segue a mesma lógica: começa pela lista de postings do
termo mais raro, que limita quantos documentos podem casar, e verifica os demais
por busca binária sobre as posições.

### 5. Trechos destacados reanalisam o documento

A alternativa seria guardar offsets por termo no índice, o que aumentaria o
índice para beneficiar apenas os 10 documentos exibidos. Reanalisar custa uma
passada sobre poucos documentos, no momento em que eles já foram escolhidos.

A janela é escolhida por número de termos **distintos**, não por total de
ocorrências: uma janela que repete a mesma palavra dez vezes explica menos o
casamento do que uma que mostra três palavras diferentes da consulta.

### 6. Indexação paralela sem perder reprodutibilidade

Analisar texto é trabalho de CPU sobre entradas independentes — o caso ideal
para paralelismo de dados. Os arquivos são divididos em blocos, cada bloco monta
um índice próprio, e os shards são fundidos ao final.

A ordem importa: os caminhos são ordenados antes da divisão e os shards são
fundidos nessa ordem, então os ids de documento são idênticos aos de uma
execução sequencial. Sem isso, empates de pontuação seriam desempatados de forma
diferente a cada execução, e nenhum teste de saída seria estável.

### 7. O arquivo de índice se descreve

Bytes mágicos e versão de formato na frente do conteúdo. Os primeiros
distinguem "isto não é um índice" de "este índice está corrompido"; a segunda
transforma uma mudança de layout em erro claro em vez de leitura errada e
silenciosa.

A gravação passa por arquivo temporário e termina em `rename`, que é atômico no
mesmo sistema de arquivos. Uma interrupção no meio da escrita deixa o índice
antigo intacto.

O analisador **não** é serializado. Lista de stopwords é configuração, não dado:
congelá-la no arquivo tornaria impossível mudá-la sem reindexar tudo.

### 8. Um arquivo ilegível aborta a indexação

O oposto — pular e seguir — produziria um índice silenciosamente incompleto. Um
motor de busca que mente sobre sua cobertura é pior do que um que se recusa a
subir.

## Complexidade

| Operação | Custo |
|----------|-------|
| Indexar um documento | O(t), t = número de termos |
| Fundir um shard | O(p), p = postings do shard |
| Consulta de termo | O(df) para coletar + O(k) para o heap de saída |
| Interseção de n termos obrigatórios | O(Σ df) com conjuntos de hash |
| Frase de n termos | O(df_raro × n × log(tf)) — busca binária por posição |
| Trecho destacado | O(t) por documento exibido |

## O que ficou de fora, e por quê

- **Compressão de postings** (delta + varint): reduziria o índice bastante, mas
  exigiria decodificação em toda leitura. Sem um corpus grande de verdade para
  medir, seria otimização especulativa.
- **Atualização incremental**: exige segmentos imutáveis com merge em segundo
  plano, tombstones para exclusão e um gerenciador de commits. É outro projeto.
- **WAND / block-max**: só compensa quando `k ≪ número de candidatos`, situação
  que este corpus não alcança.
- **Busca por prefixo e tolerância a erro**: pedem uma estrutura diferente (FST
  ou automato de Levenshtein) ao lado do índice invertido.
