# Frases e posições

A busca por frase exata verifica adjacência. Cada termo da frase tem um
deslocamento relativo ao início dela, e um documento só casa quando as posições
guardadas no índice reproduzem exatamente esses deslocamentos.

Guardar deslocamentos em vez de uma sequência simples resolve um caso incômodo:
quando uma stopword é removida dentro das aspas, ela deixa um buraco. Como o
mesmo buraco existe no documento indexado, a frase continua casando.

A verificação começa pela lista de postings do termo mais raro da frase. Ela
limita quantos documentos podem casar, e o teste posicional, que é a parte cara,
roda o mínimo de vezes possível.
