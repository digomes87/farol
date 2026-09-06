# Análise de texto

O analisador é o contrato entre indexação e consulta. Se os dois lados
produzirem termos diferentes para a mesma palavra, a busca simplesmente não
encontra nada, e o erro é silencioso.

A normalização derruba acentos e caixa, de modo que ação, AÇÃO e acao viram o
mesmo termo. As stopwords — artigos, preposições, conjunções — são descartadas
porque aparecem em quase todo documento e inflam as listas de postings sem
acrescentar sinal.

O stemming corta sufixos para aproximar flexões da mesma raiz. É uma
heurística, não uma análise morfológica: o objetivo é apenas que plural e
singular caiam no mesmo termo dos dois lados do índice.
