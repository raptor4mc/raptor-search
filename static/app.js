const SUPABASE_URL = "https://nxipygonwjlxozfsrbsn.supabase.co";
const SUPABASE_ANON_KEY = "sb_publishable_lt5n95QnheWul-URwQHtog_XBHya10T";

const input = document.getElementById("searchInput");
const button = document.getElementById("searchButton");
const results = document.getElementById("results");
let currentPage = 0;

async function search(page = 0) {
    const query = input.value.trim();
    if (query === "") return;
    currentPage = page;
    results.innerHTML = "Searching...";
    try {
        const response = await fetch(
            `${SUPABASE_URL}/rest/v1/rpc/search_pages`,
            {
                method: "POST",
                headers: {
                    "Content-Type": "application/json",
                    "apikey": SUPABASE_ANON_KEY,
                    "Authorization": `Bearer ${SUPABASE_ANON_KEY}`
                },
                body: JSON.stringify({ query: query, page_num: page })
            }
        );
        const data = await response.json();
        results.innerHTML = "";
        if (!data.length) {
            results.innerHTML = page === 0 ? "<p>No results found.</p>" : "<p>No more results.</p>";
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
        results.innerHTML = "<p>Search failed.</p>";
        console.error(e);
    }
}

button.onclick = () => search(0);
input.addEventListener("keydown", e => {
    if (e.key === "Enter") search(0);
}); //should work right now
