//! Presentation only. Original evidence remains unchanged in the journal and logs.
pub(crate) struct Explanation<'a> {
    pub summary: &'a str,
    pub action: &'a str,
}

pub(crate) fn explain(details: &str) -> Explanation<'_> {
    let (summary, action) = if details.contains("Переміщення з FAT/FAT32") {
        (
            "Переміщення з FAT/FAT32 через VartaCopy поки недоступне.",
            "Оберіть копіювання з перевіркою або передайте переміщення через Windows. Джерела не видалялися.",
        )
    } else if details.contains("FileIdInfo")
        && (details.contains("FAT32") || details.contains("exFAT") || details.contains("FAT:"))
    {
        (
            "VartaCopy поки не може виконати цю передачу на носії FAT/exFAT.",
            "Передайте файли через Windows кнопкою нижче або оберіть Explorer у вікні вибору обробника.",
        )
    } else if details.contains("os error 112)") || details.contains("os error 39)") {
        (
            "На носії недостатньо вільного місця.",
            "Звільніть місце або виберіть інший носій і повторіть передачу.",
        )
    } else if details.contains("os error 223)") {
        (
            "Файл завеликий для файлової системи призначення.",
            "Для файла розміром від 4 ГіБ виберіть носій NTFS або exFAT.",
        )
    } else if details.contains("os error 32)") || details.contains("os error 33)") {
        (
            "Файл зайнятий іншою програмою.",
            "Закрийте програму, яка використовує файл, і повторіть операцію.",
        )
    } else if details.contains("os error 5)") {
        (
            "Немає дозволу на доступ до файла або папки.",
            "Перевірте права доступу та захист носія від запису.",
        )
    } else if details.contains("os error 2)") || details.contains("os error 3)") {
        (
            "Файл або папку не знайдено.",
            "Перевірте шлях і чи підключено потрібний носій.",
        )
    } else if details.contains("os error 21)") || details.contains("os error 1167)") {
        (
            "Носій недоступний або від’єднаний.",
            "Підключіть носій і відновіть операцію з історії.",
        )
    } else if details.contains("os error 80)") || details.contains("os error 183)") {
        (
            "У призначенні вже є файл або папка з такою назвою.",
            "Виберіть правило «Зберегти обидва» або іншу папку призначення.",
        )
    } else if details.contains("WAL checkpoint is busy") {
        (
            "Не вдалося завершити обслуговування журналу: він зайнятий.",
            "Закрийте інші вікна VartaCopy та відкрийте програму знову. Стан передачі перевірте в історії.",
        )
    } else if details.contains("os error 87)") {
        (
            "Windows відхилила запит на передавання файлів.",
            "Перевірте шляхи й носій. Точний етап збою та код Windows є в технічних деталях і журналі діагностики.",
        )
    } else if details.starts_with("operation cancelled") {
        (
            "Операцію зупинено.",
            "Для продовження виберіть її в історії та натисніть «Відновити».",
        )
    } else if details.starts_with("verification failed:") {
        (
            "Перевірка файлів виявила невідповідність.",
            "Перевірте джерело й носій. Подробиці вказують файл та етап перевірки.",
        )
    } else if details.is_ascii() || details.starts_with("journal ") {
        (
            "Не вдалося виконати операцію.",
            "Перегляньте технічні деталі та журнал діагностики, перш ніж повторювати передачу.",
        )
    } else {
        // Already localized notices retain their useful context.
        (details, "")
    };
    Explanation { summary, action }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_codes_stay_in_details_and_do_not_collide() {
        for (raw, expected) in [
            (
                "The parameter is incorrect. (os error 87)",
                "Windows відхилила",
            ),
            ("disk full (os error 112)", "недостатньо"),
            ("sharing violation (os error 32)", "зайнятий"),
            ("access denied (os error 5)", "дозволу"),
        ] {
            let result = explain(raw);
            assert!(result.summary.contains(expected));
            assert!(!result.summary.contains("os error"));
            assert!(!result.action.is_empty());
        }
        assert!(!explain("failure (os error 50)").summary.contains("дозволу"));
        assert!(
            explain("FAT32: FileIdInfo Windows: os error 87")
                .summary
                .contains("FAT")
        );
        assert!(!explain("NTFS: os error 87)").summary.contains("FAT"));
    }
}
