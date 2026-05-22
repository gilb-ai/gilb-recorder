# Gilb (Gilbreth)

Приложение, которое записывает действия пользователя через accessibility API
(macOS + Windows; Linux вне scope).

## Структура репо

- `plan.md` — основной план проекта Gilb.
- `tauri-plan.md` — план Tauri-имплементации.
- `spec.md` — спецификация Tauri-архитектуры.
- `research/` — research-документы (архитектурные обзоры, рекомендации,
  разбор reference-проектов).
- `reference/` — сторонние проекты, которые мы изучаем и из которых копируем
  подходы. **Не наш код**, **не коммитится** (см. `.gitignore`). Каждая
  подпапка обычно сама по себе git-репозиторий (клон upstream'а).
- `.zenflow/` — рабочее состояние zenflow (не коммитится).

## Работа с `reference/`

- `reference/` исключён из git. Обновление выполняется как обычный pull
  в соответствующем клоне: `cd reference/<project> && git pull`.
- Документы в `research/` могут ссылаться на пути внутри
  `reference/<project>/...` — это допустимо и ожидаемо.
- Если нужно скопировать кусок кода из reference в gilb — копируй явно
  в исходники gilb с указанием источника в commit message.

Текущие reference-проекты:

- `reference/screenpipe` — Rust workspace + Tauri desktop app, источник
  подходов к захвату a11y / экрана / событий. Разбор см. в `research/`.
