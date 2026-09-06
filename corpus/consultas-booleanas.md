# Consultas booleanas

Uma consulta é uma combinação de cláusulas. Termos soltos são opcionais e
contribuem para a pontuação. Um termo obrigatório filtra a coleção: documentos
sem ele são descartados antes do ranqueamento. Um termo excluído remove os
documentos que o contêm.

A ordem de avaliação importa para o desempenho. Quando existe cláusula
obrigatória, o conjunto de candidatos vem da interseção das listas
correspondentes, que é sempre menor que a união. Pontuar poucos documentos é
mais barato do que pontuar todos e descartar depois.

Uma consulta feita só de exclusões não faz sentido: ela descreve o que não se
quer sem dizer o que se procura.
