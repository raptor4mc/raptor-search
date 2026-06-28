

const input = document.getElementById("searchInput");
const button = document.getElementById("searchButton");
const results = document.getElementById("results");

async function search(){

    const query = input.value.trim();

    if(query === "")
        return;

    results.innerHTML = "Searching...";

    try{

        const response =
            await fetch("/search?q=" + encodeURIComponent(query))

        const data = await response.json();

        results.innerHTML = "";

        if(data.length === 0){

            results.innerHTML =
                "<p>No results found.</p>";

            return;
        }

        for(const page of data){

            const div = document.createElement("div");

            div.className = "result";

            div.innerHTML = `
                <a href="${page.url}" target="_blank">
                    ${page.title}
                </a>

                <p>${page.url}</p>

                <p>${page.snippet}</p>
            `;

            results.appendChild(div);
        }

    }
    catch(e){

        results.innerHTML =
            "<p>Could not contact backend.</p>";

        console.error(e);
    }

}

button.onclick = search;

input.addEventListener("keydown", e => {

    if(e.key === "Enter")
        search();

});
