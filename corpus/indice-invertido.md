# Índice invertido

Um índice direto responde "quais termos existem no documento 7?". Busca precisa
da pergunta contrária: "quais documentos contêm a palavra ferrugem?". Por isso o
índice é invertido — cada termo aponta para a lista ordenada de documentos onde
aparece.

A lista de postings guarda também as posições de cada ocorrência. Sem elas não é
possível responder a uma consulta de frase exata, porque saber que duas palavras
estão no mesmo documento não diz nada sobre estarem lado a lado.

O tamanho de cada documento é guardado junto com os metadados. O ranqueamento
usa esse número para penalizar textos longos, que têm mais chance de conter
qualquer palavra por acaso.
