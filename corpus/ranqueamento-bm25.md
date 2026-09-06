# Ranqueamento com BM25

Encontrar documentos é a parte fácil. Ordená-los é o problema de verdade.

O BM25 combina três sinais. A frequência do termo no documento conta, mas com
retorno decrescente: dez ocorrências de ferrugem não tornam um texto dez vezes
mais relevante. A raridade do termo na coleção conta mais ainda, porque uma
palavra presente em todos os documentos não distingue nada. E o tamanho do
documento entra como penalidade, já que uma ocorrência dentro de um bilhete é
evidência mais forte do que a mesma ocorrência dentro de um livro.

O parâmetro k1 controla a saturação da frequência. O parâmetro b controla o peso
da normalização por tamanho: em zero, documentos longos e curtos competem em pé
de igualdade.
