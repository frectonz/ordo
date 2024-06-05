import { test, expect } from "@playwright/test";
import { createRoom } from "./utils";

test("whole flow", async ({ page: roomPage, browser }) => {
  await roomPage.goto("/");

  const name = "best always sunny character";
  const characters = ["Charlie", "Frank", "Sweet Dee", "Dennis", "Mac"];
  await createRoom(roomPage, name, characters);

  await expect(roomPage.getByText(name)).toBeVisible();
  for (const c of characters) {
    await expect(roomPage.getByText(c)).toBeVisible();
  }

  await expect(roomPage.getByText("APPROVE AT LEAST ONE VOTER TO BE ABLE TO START VOTES.")).toBeDisabled();

  const joinAddress = (await roomPage
    .getByTestId("voter-link")
    .textContent()) as string;

  const visitorContext = await browser.newContext();
  const visitorPage = await visitorContext.newPage();
  await visitorPage.goto(joinAddress);

  await expect(visitorPage).toHaveTitle("Join Room - ORDO");
  await expect(visitorPage.getByText(`JOIN THE "${name}" ROOM`)).toBeVisible();

  const joinRoom = visitorPage.getByTestId("join-room");
  await joinRoom.click();

  await visitorPage.waitForURL("/voters/*");

  await expect(visitorPage.getByText(name)).toBeVisible();

  await expect(roomPage.getByTestId("voter-count")).toHaveText("1");
  await expect(visitorPage.getByTestId("voter-count")).toHaveText("1");

  await expect(roomPage.getByRole("button", { name: "APPROVE", exact: true  })).toBeVisible();
  await expect(visitorPage.getByText("WAITING TO BE APPROVED.")).toBeVisible();

  await roomPage.getByRole("button", { name: "APPROVE", exact: true  }).click();

  await visitorPage.getByText("VOTER HAS BEEN APPROVED.").waitFor({ state: "visible" });

  await expect(roomPage.getByText("APPROVED")).toBeDisabled();
  await expect(roomPage.getByText("START VOTE")).toBeVisible();

  await roomPage.getByText("START VOTE").click();
  await visitorPage.getByText("START VOTING").waitFor({ state: "visible" });

  await expect(visitorPage.getByText("SUBMIT VOTE")).toBeVisible();

  await expect(roomPage).toHaveTitle("Vote Started - ORDO" );
  await expect(roomPage.getByTestId("votes-count")).toHaveText("0");
  await expect(roomPage.getByText("recorded votes")).toBeVisible();
  await expect(roomPage.getByText("AT LEAST ONE RECORDED VOTE REQUIRED TO BE ABLE TO END VOTES.")).toBeDisabled();
  await expect(roomPage.getByText("WAITING")).toBeVisible();

  await visitorPage.getByText("SUBMIT VOTE").click();
  await visitorPage.getByText("THANKS FOR VOTING!").waitFor({ state: "visible" });

  await expect(roomPage.getByTestId("votes-count")).toHaveText("1");
  await expect(roomPage.getByText("recorded vote")).toBeVisible();
  await expect(roomPage.getByText("END VOTE")).toBeVisible();
  await expect(roomPage.getByText("VOTED")).toBeVisible();

  await roomPage.getByText("END VOTE").click();
  await visitorPage.getByText("VOTES HAVE ENDED.").waitFor({ state: "visible" });

  await expect(roomPage).toHaveTitle("Vote Ended - ORDO" );
  await expect(roomPage.getByText(`RESULTS FOR "${name}"`)).toBeVisible();
  await expect(roomPage.getByText("SCORE")).toBeVisible();
  for (const c of characters) {
    await expect(roomPage.getByText(c)).toBeVisible();
  }

  await visitorContext.close();
});
