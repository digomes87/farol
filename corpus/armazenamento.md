# Armazenamento em disco

Construir o índice é a parte cara. Relê-lo deve ser barato, então o índice é
serializado inteiro e guardado em um único arquivo.

A gravação passa por um arquivo temporário e termina com uma renomeação atômica.
Sem isso, uma interrupção no meio da escrita deixaria um índice pela metade no
lugar de um índice válido.

Na frente do conteúdo ficam bytes mágicos e um número de versão de formato. Os
primeiros distinguem "isto não é um índice" de "este índice está corrompido"; o
segundo transforma uma mudança de layout em erro claro, e não em leitura errada
e silenciosa.
