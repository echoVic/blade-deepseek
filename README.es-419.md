# Orca

Un agente de programación nativo para DeepSeek, diseñado para tu terminal.

Dale una tarea a Orca y leerá el código, editará archivos, ejecutará comandos,
verificará el resultado y seguirá trabajando hasta terminar o necesitar tu decisión.
Usa la TUI para trabajo interactivo o `orca exec` para scripts y CI. Orca está
escrito en Rust, se ejecuta localmente y usa la licencia MIT.

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[Sitio web](https://orcaagent.dev/) · [Cambios](https://orcaagent.dev/changelog/) · [Versiones](https://github.com/echoVic/blade-deepseek/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## Instalación

```bash
npm install -g @blade-ai/orca
```

También puedes instalar directamente el binario nativo:

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

El paquete npm es compatible con macOS y Linux en ARM64 y x64. También hay
archivos precompilados en [GitHub Releases](https://github.com/echoVic/blade-deepseek/releases/latest).

## Uso

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # abrir la TUI
orca exec "corrige la prueba que falla"   # ejecutar sin interfaz
orca exec --verifier "cargo test" "corrígelo" # verificar antes de terminar
orca --mode=acp                           # conectar un cliente ACP
```

En la TUI, `@` busca archivos, Skills, Plugins y MCP Resources. Usa `/plan`
para planificación de solo lectura, `/goal` para un objetivo persistente,
`/workflows` para trabajo en segundo plano y `/trust` para administrar los
permisos del sandbox de la carpeta actual.

## Funciones principales

- Usa directamente la semántica de razonamiento y herramientas de DeepSeek, con
  streaming SSE, prompts compatibles con caché de prefijos, gestión automática
  del contexto y reintentos.
- Lee, busca, edita y escribe código; ejecuta comandos de shell y verifica el
  resultado con el comando que elijas.
- Controla acciones de riesgo con `suggest`, `auto-edit` dentro del sandbox,
  `full-auto` con acceso total, `plan` de solo lectura y confianza por carpeta.
- Guarda el historial local con reanudación, bifurcación, búsqueda, cambio de
  nombre, archivo y compresión.
- Ejecuta objetivos persistentes sin un límite fijo de turnos, subagentes y
  workflows de JavaScript para tareas que requieren continuidad o trabajo paralelo.
- Carga instrucciones, Skills, Plugins, herramientas personalizadas y recursos
  MCP después de confiar en el workspace.
- Ofrece contratos JSONL, app-server y Agent Client Protocol (ACP) estables
  para editores, harnesses y CI.

La prioridad de configuración es: variables de entorno, argumentos de CLI,
archivos de configuración y valores predeterminados. Ejecuta `orca --help` o
`orca exec --help` para ver todos los comandos. La configuración del usuario
está en `~/.orca/config.toml`; los proyectos de confianza también pueden incluir
`.orca/config.toml`, `AGENTS.md`, reglas, Skills y workflows.

Más información:

- [Persistent Goal Mode](docs/goal-mode.md)
- [Contrato del harness y app-server](docs/harness-contract.md)
- [Diseño de workflows dinámicos](docs/claude-code-workflow-parity.md)
- [Hoja de ruta de producción](docs/production-roadmap.md)

## Comunidad

- Grupo de QQ: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## Contribuciones

Lee [CONTRIBUTING.md](CONTRIBUTING.md) antes de contribuir. Abre primero un Issue
para cambios grandes o que puedan afectar la compatibilidad.

- [Reportar un error](https://github.com/echoVic/blade-deepseek/issues/new?template=bug_report.yml)
- [Proponer una función](https://github.com/echoVic/blade-deepseek/issues/new?template=feature_request.yml)
- [Obtener ayuda](SUPPORT.md)
- [Reportar una vulnerabilidad](SECURITY.md)

## Licencia

[MIT](LICENSE)
