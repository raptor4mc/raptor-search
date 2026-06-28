const input = document.getElementById("searchInput");
const button = document.getElementById("searchButton");
const results = document.getElementById("results");
let currentPage = 0;
let currentQuery = "";

async function search(page = 0) {
    const query = input.value.trim();
    if (query === "") return;
    currentQuery = query;
    currentPage = page;
    results.innerHTML = "Searching...";
    try {
        const response = await fetch(
            "/search?q=" + encodeURIComponent(query) + "&page=" + page
        );
        const data = await response.json();
        results.innerHTML = "";
        if (data.length === 0 && page === 0) {
            results.innerHTML = "<p>No results found.</p>";
            return;
        }
        if (data.length === 0) {
            results.innerHTML = "<p>No more results.</p>";
            return;
        }
        for (const item of data) {
            const div = document.createElement("div");
            div.className = "result";
            div.innerHTML = `
                <a href="${item.url}" target="_blank">${item.title}</a>
                <p class="url">${item.url}</p>
                <p>${item.snippet}</p>
            `;
            results.appendChild(div);
        }
        const nav = document.createElement("div");
        nav.className = "pagination";
        if (currentPage > 0) {
            const prev = document.createElement("button");
            prev.textContent = "← Previous";
            prev.onclick = () => search(currentPage - 1);
            nav.appendChild(prev);
        }
        if (data.length === 10) {
            const next = document.createElement("button");
            next.textContent = "Next →";
            next.onclick = () => search(currentPage + 1);
            nav.appendChild(next);
        }
        results.appendChild(nav);
    } catch (e) {
        results.innerHTML = "<p>Could not contact backend.</p>";
        console.error(e);
    }
}

button.onclick = () => search(0);
input.addEventListener("keydown", e => {
    if (e.key === "Enter") search(0);
});
